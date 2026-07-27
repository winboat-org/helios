# ROADMAP — Stage: Performance, Stability, Conformance (PSC)

*The desktop first rendered end-to-end on 2026-07-05. The active architecture
changed on 2026-07-09: Helios is now a WDDM render+display adapter and owns the
virtio-gpu scanout; IddCx/Looking Glass is no longer the active display path.*

## Current verified baseline (2026-07-23, KMD 22.22.142.0)

- `DisplayHalf=1` exposes one connected child and one VidPn source. DWM composes
  the whole desktop on Helios and `SetVidPnSourceAddress` selects the real
  primary for `SET_SCANOUT_BLOB`.
- `ScanoutDiag` is **deleted/off**. Mode 16 remains a diagnostic only and must
  never overwrite the real primary during a desktop test.
- The LINEAR diagnostic image is proven on NVIDIA. The old failure was a guest
  constant bug (`VK_IMAGE_TILING_LINEAR` was encoded as `0`; it is `1`). After
  the fix, same-boot breadcrumbs reached `SdgLStg=0x10`, host-visible/coherent
  memory was selected, and the owner saw its fill pattern in VNC.
- The real DWM primary is a dedicated, DMA_BUF-exportable Venus
  `VK_IMAGE_TILING_OPTIMAL` allocation. The UMD marks the actual
  `CDD_SHAREDPRIMARYSURFACE`; the KMD uses that allocation in
  `SetVidPnSourceAddress`. There is no heuristic selection and no guest-side
  primary-to-scanout copy.
- The QEMU fork propagates virglrenderer DMA_BUF modifier metadata and the
  existing `RESOURCE_CREATE_BLOB.size` internally, without changing the public
  virtio-gpu wire ABI. Plain OPTIMAL exports currently arrive as
  `DRM_FORMAT_MOD_INVALID`; EGL cannot describe that layout. QEMU reconstructs
  the exact producer VkImage, verifies its Vulkan memory requirement equals the
  original blob allocation size, copies image-to-staging on the host GPU, and
  publishes a CPU `DisplaySurface` to VNC. This is direct guest-primary scanout,
  but **not end-to-end zero-copy** because the host display backend reads back.
- Visible desktop output is verified. A DComp scheduled-task probe completed
  1576 Presents in 25 seconds (63.0 fps), and interaction was responsive while
  that continuous producer ran. This isolated the perceived lag to the
  idle-to-active scanout edge, not steady-state GPU throughput. The UMD now
  emits a refresh marker after the exact DWM primary operation. KMD
  `DxgkDdiRender` captures the current Venus wire-fence watermark under the
  statically witnessed notification lock; the used-ring DPC coalesces markers
  and dirties scanout only after all preceding Venus work retires. This does not
  depend on VidSch choosing `SubmitCommand` versus `SubmitCommandVirtual`.
- The v142 wake test advanced the live 16-refresh telemetry snapshot
  (`AsSub`/`AsDone` caught up, `WtOut=CtOut=QfRet=0`). Same-boot QEMU evidence
  then rebound the real 1896x1030 OPTIMAL primary and completed Vulkan readback
  in about 1.0–1.9 ms. The owner confirmed excellent idle-to-active
  responsiveness.
- The KMD watermark orders Venus commands which already exist when the marker
  reaches `DxgkDdiRender`; it cannot cover work still queued on DXVK's
  submission thread. With `PresentGateUs=0`, fast cursor motion exposed that
  producer race as stale cursor replicas. A 5 ms A/B still leaked six stale
  frames in one 128-present burst, so the direct-primary default is now a
  bounded 10 ms `HeliosWaitFrameComplete` before the kernel present callback.
  It sleeps on DXVK's submission-fence condition variable instead of polling.
  The 10 ms A/B measured 0.48 ms cumulative average after 384 presents and
  zero timeouts after its six startup expirations. The owner confirmed both
  excellent responsiveness and no cursor ghosting.
- The old synchronous KMD `RESOURCE_FLUSH` control roundtrip is gone from the
  frame path. One interrupt-completed async bind/flush is allowed in flight and
  later flips coalesce. Control DMA buffers are reaped/reused outside the
  spinlock. Mesa's Windows ring notifies an idle renderer eagerly, folds
  side-effect-free wait-only timeline submits on the guest, and reuses its
  escape staging buffer; per-submit shape logging is opt-in.
- The same exact OPTIMAL Vulkan fallback is shared by `egl-headless`, GTK EGL,
  GTK GLArea, and SDL OpenGL. `egl-headless`+VNC and SDL OpenGL on native
  Wayland are visually verified. The launcher leaves interactive EGL vendor
  selection to the compositor while pinning Venus/readback Vulkan to NVIDIA.
  GTK/Wayland still fails during the full run with repeated GDK
  `eglMakeCurrent` errors and remains unverified.

## Current priorities

1. **DONE (2026-07-26) — `REFACTOR_REVIEW.md`.** The parallel adversarial review of
   `kmd_render` and `umd` (including the cxx `dxvk_bridge`) is complete: 300 verified
   findings, 177 recommendations, eleven dependency-ordered tranches T0–T8, each with
   its own regression gate. A refute-first verification pass re-opened every cited
   line and rewrote 185 findings, refuted 3, and settled the high-severity count at
   26. Three results shape the plan: there are **no unit tests** in either crate, a
   default deploy ships and measures the **debug** UMD, and many failure breadcrumbs
   are invisible at the default `DiagLevel`. The review also carries the
   implicit-ordering register and the static-guarantee catalogue (organised by
   mechanism, with an explicit *rejected as cosmetic* list).
2. **T0 DONE + gate PASSED (2026-07-26), KMD `22.22.178.0`.** All five T0 recommendations
   landed: `kmd_logic/` is a dependency-free host-testable `no_std` crate holding the
   page/pitch arithmetic (5 tests; `kmd_render` cannot host a libtest harness at all —
   bindgen + `rc.exe` in build.rs, `panic="abort"` cdylib); the deploy default is the
   **release** UMD and dwm is now proven to load `umd/target/release/helios_umd.dll` out
   of the DriverStore; the KMD version is one line in `kmd_render/driver-version.env`,
   with the coherence gate in `build.rs` (`verify_version_wiring`) so a hand-run
   `cargo make` is covered and **building no longer depends on win-mcp**; `umd` names its
   Windows-only constraint; and the C++ toolchain identity is a declared cargo build
   input with four existence-checked paths. Release-UMD baseline recorded in
   `REFACTOR_REVIEW.md` Appendix A. **Deploy trap learned:** a rebuild at an
   already-published version can never produce a verifiable DriverStore image — the
   install script re-signs the package after the build signed it, so devcon's no-op
   publish leaves a hash mismatch and the bind check (correctly) refuses. Bump first.
3. **T1a DONE + gate PASSED (2026-07-26), KMD `22.22.179.0`.** Sixteen wedge-class KMD fixes
   (R201–R216), one recommendation per commit. The class they share is a silent stop:
   the `vidpn_programming` gate latching at 1 (DestroyAllocation cancel, the ScanoutDiag
   hook, and state carried across StopDevice) which kills the CRTC_VSYNC heartbeat; the
   `failed` ring latch making the WDDM pending FIFO undrainable into a TDR loop; the
   one-shot Venus copy-target latch that failed every scanout after a resize; a WDDM
   fence popped before its notification was attempted; the dangling `MmMapIoSpace`
   sentinel published as a valid ISR register; an unbounded HPD-worker join whose failure
   freed a context a live thread still touched. Every one of them was previously invisible
   at the default `DiagLevel`, which is why R201 (ungated `diag::fault` + `FaultCounter`)
   landed first. **15 new fault counters, all zero on a healthy boot.**
   ⚠ **`VsCnt`/`SaCnt` are mirrored to the registry only from the scanout pacing snapshot** —
   on an idle desktop they stop being written and a short sample reads delta 0. Provoke
   presentation before trusting them. **As of 22.22.180.0 (R318) that snapshot runs OUTSIDE
   the scanout mutex and at ONE rate, `n==1 || n%600==0` (was `n%16`), so the provocation
   must be long enough to cross the period** — a 25 s `helios_dcomp_probe` run (~1250
   refreshes at ~50 fps) crosses it twice.
4. **T1b LANDED — gate PASSED (2026-07-26, KMD `22.22.180.0`, 24 commits).** Both halves in one
   image, bug commits before telemetry commits so the cadence numbers were taken against a
   functionally known-good driver.
   **⚠ SCOPE CHANGE, owner directive: the GDI path is RETIRED, not hardened.** Helios does not and
   should not advertise GDI acceleration, so T6's `R903`/`x-dup-dead-20` was pulled forward:
   `SupportKernelModeCommandBuffer` is hard-coded 0, the `GdiAccelMode` knob is gone, and
   `gdi_blit.rs` (819 lines: batch parser, `MmMapIoSpace` view cache, six CPU rasterizers) is
   deleted. **T1b's R301/R304/R305/R306 died with it** — all four only hardened that executor.
   Reachability was re-proven the same boot BEFORE deleting: every `Gd*` service value deleted,
   then explorer restart + maximized notepad + GDI canary + repaint + EnumWindows + two paintcaps,
   and **not one reappeared** (the executor flushed its block on its first batch, so absence is
   proof, not throttling).
   Landed: R302 (`PagingOpOutcome` + one `paging_failure()`; the crate's last `STATUS_UNSUCCESSFUL`
   is gone), R303 (`MdlWindow` bounds the eviction WRITE side), R307, R308 (`MetaLayout` per-arm
   trailer), R309 (one aperture validator, refusal handling split by IRQL), R310, R311 + R312
   (`DeviceOwner(NonZeroUsize)`; context tracking now reserve-then-commit so the owner check is
   authoritative), R313 (seqlock, bounded retry), R314/R315/M10 (QUERY_STATS **V3**, appended),
   R316–R320, and M1–M8/M10. Five new host-tested `kmd_logic` helpers (`window_range`, `Pfn`,
   `MetaLayout`, `seq_read`, + tests): **17 tests green**.
   **Gate evidence, all same boot:** cold boot → `CM_PROB_NONE` on `22.22.180.0`, visible desktop;
   3 × `pnputil /restart-device` all rebind `CM_PROB_NONE` (the R903 cap change is the one that
   could have reproduced FAILED_ADD). Every fault counter 0; every new refusal counter 0/absent
   **except the two deliberately provoked**. Attacks (`tools/escape_owner_probe.c --attack`):
   owner==0 `RELEASE_BLOB` against the live scanout resource → `0xc000000d` refused, `EscNoDev` +1,
   `blobs_live` unchanged, **DWM alive**; cross-device `CTX_DESTROY` → `0xc0000010` refused,
   `EscCtxOwn` +1, while the owner's own `CTX_DESTROY` still succeeds; bad magic and verb `0x0007`
   each +1. `DiagLevel=0` still shows `StVio`/`StBar`/`PgTs`/`PgTd`/`BAR_ERR_*`; every `PB*` name
   still present at its sampled cadence (`PBcall` steps in exact 600s), and `DiagLevel=1` restores
   per-call (`PBcall` 610→904 in 6 s). R320 proven live — `PresentProbe` is ON on this box and the
   DEFERRED probe still reaches `PBPrF=16` with `PBPrNz=64 PBPrSum=12616`.
   **Measured, same procedure before and after:** DComp **46.6 / 46.5 → 51.6 / 49.2 fps**; dwm
   present-gate **avg 2241 → 1407 µs**, timeouts **844/14464 (5.8 %) → 87/6784 (1.3 %)**.
   **⚠ NOT PROVEN — the paging/eviction half never ran.** `PgTi`/`PgTo` are 0 for the whole boot:
   VidMm evicted nothing (Fire Strike was killed mid-run by the owner and its relaunch lost its
   controller), so R302/R303/M8's new failure paths are **unexercised on hardware** — the zeros
   prove no regression, not that the new refusals behave. Same for R309's DISPATCH arm (`ChMc`=0:
   no aperture maps this boot). Next session should force real eviction pressure (a completed Fire
   Strike run plus a working-set larger than the 1 GiB BAR partition) before trusting them.
   Cursor-trail check not performed (needs interactive mouse input).
5. **T2 LANDED — gate PASSED (2026-07-27, release UMD only, 33 commits on `wddm`).** All 30 items
   R401–R430. **No KMD change: still `22.22.180.0`, no reboot.** Bug half landed and was verified on
   hardware BEFORE the telemetry half, so the cadence numbers were taken against a functionally
   known-good driver.
   Landed: R401 (`finish_create` — VOID `Create*` failures now reach `set_runtime_error`, worst case
   the direct-scanout PRIMARY), R402 (`OpenedAllocation` makes the meta trailer non-optional, so the
   1×1 BGRA alias is unrepresentable), R403, R404 (`DeviceUnderConstruction` drop guard;
   `create_runtime_context` returns an HRESULT), R405 (`NegotiatedInterface` + a `const` assert
   against `SUPPORTED_DDI_VERSIONS`), R406, R407, R408 (+ a real TEX1D arm), R409, R410 (`DdiSlice`
   + `collect_slots`), R411 (`bridge_guard`, 7 methods, `noexcept` in the header), R412–R415
   (`PresentReady`, `rotate_ring`, `GateOutcome`, one `VehicleSlot` + a device-liveness refusal),
   R416 (`HeliosIcdExports`, successes cached only), R417 c1, R418, R419, R420, R421, R422–R430.
   **Gate evidence, all same boot:** desktop composited (paintcap ×3); Fire Strike ran to completion
   and stored a result; the D3D11 knob + extra probe suites all `rc=0`, `upload_integrity` **30/30
   PASS, 0 mismatches**, `shared_content`/`shared_draw`/xproc cross-process draw all PASS. **ZERO
   refusals of any kind** — no create failure, no `no meta trailer`, no `unsupported interface`, no
   `DDI noop` hit, no `suspicious slot`, no DXBC rejection — across idle + Fire Strike + 2 DComp
   probes + both suites. dwm **modules flat at 81** and handles 1064→1061 over a ~10-minute soak
   (the R416 `LoadLibraryA` / COM-leak proof). R409 proven live: `DDI scanout registry: dropped 1
   entry … remaining=0..2` — bounded, where it used to only grow.
   **Measured, identical procedure before/after (60 s idle + a 200 s Fire Strike run):**
   `UmdTrace=0` dwm log growth **3,425 → 0 bytes/min idle** and **353,818 → 35,303 bytes/min under
   Fire Strike (10×)**. DComp probe ×2: **42.4 / 44.4 → 44.7 / 50.8 fps**; present-gate avg
   **2264 → 2101 µs**. ETW `Microsoft-Windows-DxgKrnl` + `DXGI`, same 260 s workload on both builds:
   AzureTriage **1352 → 1342** events, i.e. **no new driver-bug entries**; both builds show only the
   two pre-existing KMD-side codes, `0xC0000001` ×1254 and `0xC00000BB` ×114 (defect 5 below) — and
   T2 changes no KMD code. R414 cross-check: the new `gate_nc=44` field **exactly equals** the C++
   `present-gate: … timeouts=44` read at the same point.
   ⚠ **Two review premises corrected against hardware** (both amended in `REFACTOR_REVIEW.md`):
   (a) R416 is NOT a per-frame win here — all three `present_sync_publish` call sites are gated on
   `PresentSyncPublish`, which defaults to 0 and is absent from the registry, so `residOf` never
   runs on the present thread; 2432 dwm presents produced **zero** `resid-lookup:` lines. Its real
   value is the create path plus the closed `LoadLibraryA` growth. (b) R420's `LogThrottle` cannot
   be "instantiated per site" — 11 of the 37 statics are SHARED by sites with different budgets, so
   the budget had to become a call argument; cadence identity was verified by diffing all 69 sites.
   ⚠ **Not performed:** cursor-trail check (needs interactive mouse input); dxvk-tests (not
   installed on this box — the in-tree D3D11 probe suites were the surrogate); idle-to-active wake
   not timed separately (the zero idle log growth is the mechanism the review cites for it).
   ⚠ **Pre-existing, NOT a T2 regression:** `tools/d3d11_shared_blob_truth_probe.cpp` prints
   `IDENTITY PARSE FAILED` because `parse_identity` uses `fopen_s`, which opens deny-sharing against
   a log the UMD holds open — the documented 18th-session trap. The needle it greps
   (`open_resource identity: res_id=`) is still an unconditional `log_line` and is present in the
   file. ⚠ **FIXED 2026-07-27** — `win_install_umd` could not refresh the DriverStore copy while it
   was in use (`Copy-Item … being used by another process`), so every deploy of a session silently
   left the COLD-BOOT copy stale (measured: store SHA 56473A67 vs current F0C7A2E6, eight hours and
   six deploys behind) while still reporting success. It is a SHARING violation — the package copy
   is mapped by long-lived shell processes (ShellHost, SystemSettings, CrossDeviceResume) — so
   takeown/icacls never applied. `Copy-HeliosFileVerified -DisplaceInUse` now renames the loaded
   image aside (handles follow the rename; new loads get the fresh file), inherits the SHA256
   verification, and THROWS on failure. A reboot-scheduled replace was rejected because
   `Clear-HeliosPendingRenames` strips pending helios_umd renames at every deploy.
   ⚠ **Post-gate follow-ups landed the same session:** R416 was still leaking — caching successes
   only means an absent export retries every call, and the manifest walk `LoadLibraryA`s each
   candidate without freeing on the MISS; the cache bounds the HIT path, and the leak only ever
   existed on the miss path (fixed; the gate could not catch it because every export resolves here).
   R420 was missing its static guarantee — `log_line` is now `#[deprecated]` as an internal marker
   with `log_error!`/`trace_line!` the only callers and `#![deny(deprecated)]` making a direct call a
   COMPILE ERROR (fault-injection verified), and the caps-query sites moved behind `trace_line!`.
   ⚠ **Measurement trap found while doing that:** `umd-<pid>.log` is PID-keyed and opened
   `append(true)`, so a recycled PID appends to a file an EARLIER process wrote — whole-file line
   counts mix processes. Window every histogram (`Select-Object -Skip $before`) and compare the
   file's `CreationTime` against the process `StartTime`. Byte-DELTA measurements over a fixed
   window are immune, which is why the A/B growth numbers above stand.
   ⚠ R417 commit (3) remains **owner-gated and NOT done**: `track_dwm_composition_target` is the
   ONLY writer of `dev.composition_source`, so deleting that call would silently disable both the
   legacy LINEAR copy and the `flush()` refresh-marker submission.
   ⚠ The UMD still has **no registry/escape counter surface** — a "named counter" is a process-global
   `AtomicUsize` next to the `EXT_*` block plus a field in an existing periodic dump line. New in
   T2: `skips=a/b/c` and `gate_nc=` on the `DXGI Present: #N` line, `resid-lookup:` and the
   `slots real=/noop=/calc=/null=` classification (both `UmdTrace`-gated), and
   `DDI scanout registry: dropped …`.
   ⚠ **Observable knob change (R429):** `HELIOS_PRESENT_READBACK`, `HELIOS_PRESENT_FORCE_OPAQUE` and
   `HELIOS_PRESENT_OPTIMIZE_COMPOSITION` are now read ONCE per process; setting them on a live
   process no longer takes effect.
6. **T3 LANDED — gate MOSTLY PASSED (2026-07-27, KMD `22.22.183.0`, 22 commits).** All 15 items
   R501–R515, one commit each, R515 split into its two mandated commits. Prereqs verified in T1a
   before starting: `k-display-01` (`58df173`), `k-display-03` (`a829eed`), `k-display-13`
   (`2aa3bb8`). **OWNER DECISION: T3 went before T4/T5.**
   Landed: R501 (`ScanoutGuard`; the 59-line `retire_scanout_allocation` body verified
   line-identical), R502 (`NotifyOrdered` — **correction: NINE guard-scoped `with_virtio` sites and
   SIX ordered methods, not seven/five; T1a split `take_ready_wddm`**), R503 (`ScanoutFormat` in
   `kmd_logic`; **deviation — the review's single acceptance set would have CHANGED behaviour**, so
   `from_dxgi` (strict {28,87,88}) and `from_dxgi_or_legacy_zero` (adds `0`) preserve both sets),
   R504 (RAII `ProgrammingInterval`, nine `store(0)` sites gone, one `transfer_to_completion`),
   R505 (`ScanoutReject`, 8 breadcrumb pairs + statuses diffed by hand, 9 new counters),
   R506 (`ProgrammedPrimary` + per-variant `retryable()` + 4-attempt budget), R507
   (`WindowsPrimary`/`ScanoutTarget`; **the undersize guard moved byte-identical**), R508
   (`enqueue_scanout_submit`; store order and `response_ok` asymmetry diffed unchanged), R509
   (packed `(seq<<32)|active` gate; **FOUR reader contexts — the review named one gpu.rs clear
   site, there are TWO**), R510 (`StartedState` published once; **deviation — ONE commit, not three:
   publish-once forbids continuing to mutate `dxgkrnl` through a live `&mut`, so sub-commits 1 and 2
   are not separable**), R511, R512, R513 (four RAII guards + `CofuncStage`; `VpECf` codes verified
   byte-identical), R514 (`VidPn<'a>`, **no `'static` anywhere — grep-verified**), R515 c1
   (swap-before-indicate, the tranche's declared behaviour fix) and c2 (`start_complete` edge).
   22 host tests in `kmd_logic` green.
   **Gate evidence, all same boot (`22.22.183.0` pending; measured on `.182`, code-identical apart
   from the knob name):** cold boot → `CM_PROB_NONE`, **visible composited desktop** (paintcap:
   wallpaper, taskbar, window chrome, live clock). Per-flip diag over the fixed 60 s session
   **identical to the pre-image baseline**: `VpSA` 1→1200→2400, `ScSet=1 ScFlu=3 VpDSt=0
   DspMd=124257286 ScCpy=2 ScPch=7680 ScOff=0` at every snapshot (`ScRid` differs — it is a
   per-boot resource id). All 15 new counters **0**, including `ScStale=0` (the DIRQL/PASSIVE
   interleave does not occur), `ScGateCx=0` (the DIRQL CAS never exhausted) and **`HpdStTo=0`
   (R515's real start edge fired, not the 500 ms fallback)**. `VpECf`/`VpECn` **absent** with
   `VpECp=1` (R513 clean). Every fault counter 0. **7 consecutive `pnputil /restart-device`
   cycles** all rebound `CM_PROB_NONE` with a working desktop — the R510/R511 blast radius.
   present-gate **avg 2089 µs / 0.86 % timeouts** vs 2189 µs / 0.96 % before. Same-boot QEMU:
   **18 × `OPTIMAL DMA-BUF ready 1896x1030`, `tiling=OPTIMAL size=8773632`**.
   ⚠ **NOT YET PROVEN — the forced-refusal leg.** The gate instrument (`ScForceReject`) was named
   `ScanoutForceReject` (18 chars) and `read_config_dword` truncates to 14, so it read as its
   default 0 on every boot and not one exit was forced. Fixed in `.183` (see the knob-limit entry
   below); the eight exits still need one run. Note exits 7 (`NoTarget`) and 8 (`CopyFailed`) live
   on the copy/fallback arm, which a direct primary never takes — they need a forced-fallback build,
   exactly as the review predicted.
   ⚠ **DComp cadence: ~45 fps after (44.0/46.6/46.2/45.0, four 25 s runs) vs ~52 before
   (53.4/50.4).** Within the documented 42–53 boot-to-boot spread and the `.180` measurement was on
   a 5-hour-uptime box vs a fresh boot here, so this is **neither attributed to T3 nor cleared** —
   a KMD A/B on one boot is impossible. Folds into the open WS2 cadence defect.
   ⚠ **Not performed:** suspend/resume and the cursor-trail check (owner opted to run both; still
   outstanding).
   ⚠ **BOOT REGRESSION found and fixed — `22.22.181.0` did not boot** (`0xc0000001` / Startup
   Repair, only with the virtio-gpu device present). **Kernel stack overflow in
   `DxgkDdiStartDevice`**, measured from the images: StartDevice 8824 B (.180) → **9688 B** (.181),
   and it calls `VirtioGpu::init` at 9112 B — **18800 B nested on a 24 KB kernel stack**, with
   dxgkrnl's frames above. R510's 832-byte `StartedState` (which embeds a 576-byte
   `DXGKRNL_INTERFACE`) was built and passed BY VALUE in that frame. Fixed by boxing it behind an
   `#[inline(never)] StartedState::boxed` that takes the interface as a POINTER, plus an
   `#[inline(never)] bring_up_venus`: **8376 B, nested 17488 B — below the known-good .180**.
   Diagnostic notes: it ran FINE as a live `devcon` restart (shallower caller stack), and an early
   double fault writes **no dump and logs no bugcheck event** — the absence was the clue. ⚠ **The
   pair is still ~17.5 KB of 24 KB; `VirtioGpu::init` alone is 9112 B and is pre-existing. The next
   change adding ~1 KB to either function re-breaks boot the same silent way** — shrink
   `VirtioGpu::init`, or add a build-time frame check on the boot path.
   ⚠ **Deploy trap hit:** `.181` was published before it was known bad, so its DriverStore package
   had to be `pnputil /delete-driver`'d and the service `ImagePath` **and** the display key's
   `UserModeDriverName`/`…Wow` repointed by hand — `win_install_kmd` refuses to run with the PCI
   device absent, so the script's normal fixups never happened. Sweep `HKLM\SYSTEM` for the old
   store-dir GUID after any manual driver surgery.
   ⚠ **New durable guard:** `read_config_dword` now takes `&[u8; N]` and const-asserts
   `N <= MAX_CONFIG_NAME` (14), so an over-long knob is a BUILD FAILURE instead of a knob that
   silently reads as its default forever. Two shipped knobs (`CrossAdaptCaps`, `DirectFlipCaps`)
   already sit exactly at the limit. The guard fires on `cargo build`, **not** `cargo check` — an
   inline `const` in a generic fn is evaluated at monomorphisation.
   ⚠ One consequence for T5 sequencing: T5's **R805** renames `protocol/`'s `_pad` and four
   `kmd_render` read sites, so it is not UMD-only. Doing T3 first means R805 folds naturally into a
   KMD-deploy window instead of being an awkward exception to T5's "release UMD only" shape.
7. **T4a LANDED — gate PASSED (2026-07-27, KMD `22.22.184.0`, 34 commits).** 18 of the 19
   items (R601–R613, R615–R619) plus **all 11 minor items** landed; **R614 was re-scoped to
   its own tranche and has since LANDED — see item 7b** — so T4a is complete as scoped.
   **Gate evidence, all same boot (08:40:31):** cold boot to `CM_PROB_NONE` on 22.22.184.0 with a
   visible composited desktop (`helios_paintcap`), and a second composited capture after **nine
   consecutive `pnputil /restart-device` cycles**. Per-flip diag IDENTICAL to the pre-image
   baseline — `VpSA=1 ScSet=1 ScFlu=3 VpDSt=0 DspMd=124257286 ScCpy=2 ScPch=7680`, `ScanoutDiag`
   absent. Every failure counter clear, every T4a counter absent, `ChSzMm`/`ChSzPv` 0.
   `ASYNC_SUBMIT_COUNT == ASYNC_COMPLETE_COUNT` exactly at both 332102 and 24110 (R611).
   `WtOut`/`CtOut`/`ctrl_timeouts`/`rangedrops`/`dma_fails`/`mmio_fails` 0 throughout.
   `QfRet` **63/run before vs ~61/run after** (182 across three Fire Strike runs) — R601/R616's
   backpressure is unchanged. dwm survived the whole soak; zero 4101/dxgkrnl/LiveKernelEvent
   entries in the System log. Host log: 54 × `OPTIMAL DMA-BUF ready 1896x1030`, same-boot ones on
   this image, and **no venus decode, "failed to look up object", or validation lines anywhere**.
   **Measured:** DComp 57.8/50.0 → **58.0/48.5 fps** (unchanged); dwm present-gate
   **avg 2099 → 1854 µs, timeouts 0.91 % → 0.56 %** (better). Fire Strike Graphics 21024 (n=1,
   pre) vs 19460/20312/20150 (n=3, post) — **~5 % lower and NOT ATTRIBUTED**: one baseline sample
   against three, on a host GPU shared with the compositor, and the per-frame present-gate metric
   moved the other way. Re-baseline it before reading anything into it.
   **T3's outstanding forced-refusal leg is DISCHARGED**, and the reason it could not be
   discharged in T3 is now known: `record_scanout_reject_counters` is reachable only from
   `pacing_snapshot`, which is driven by the count of SUCCESSFUL refreshes — force every
   `SetVidPnSourceAddress` to refuse and the counters never reach the registry at all. Checking
   the UNGATED per-refusal breadcrumb instead, **five of six reachable exits FIRE**: 1 BadAlloc
   (`ScRid` 36→0), 2 Extent (`ScSet` 1→0xD), 3 Layout (→0xE3), 5 LinearAllocFailed (→0xE1),
   6 SetFailed (→0xE), with `ScFrc` echoing each value. **4 Format is NOT DISCRIMINABLE** — its
   breadcrumb is `ScFmt = the live dxgi_format` (88, the same value the healthy path leaves) and
   its arm writes no `ScSet`; the site sits between the two proven ones, so the path is reached.
   7/8 remain unreachable with a direct primary.
   ⚠ **NOT performed:** DOOM, suspend/resume, rapid cursor motion (needs an interactive mouse —
   now FOUR tranches overdue), and the `HELIOS_VKR_DEBUG=validate` host command-mix capture
   (needs a launcher relaunch, which is owner-owned). New host tests: `kmd_logic` 28→34, `protocol` 3→8.
   **Structure added:** the no-panic rule is now enforced twice (a `verify-no-panics` cargo-make
   task that fails the PACKAGE build, plus `#![deny(clippy::unwrap_used, expect_used, panic)]`) —
   `grep -rn '\.expect(\|\.unwrap()' kmd_render/src` is EMPTY. `Writer` moved to `kmd_logic`
   with a sticky overflow flag; six per-class Vulkan id newtypes over `NonZeroU64` (`VkImageId`,
   `VkDeviceMemoryId`, `VkBufferId`, `VkCommandPoolId`, `VkCommandBufferId`, `VkFenceId`) plus
   `VkDeviceId`/`VkQueueId` from the bring-up typestate; `VenusRing → VenusInstance →
   VenusClient`; `RingMap`/`RingWord`; `Chain`/`DmaSpan`; `SyncWaitBlock::with` +
   `SyncTicket`/`SyncOutcome`; `AllocationBacking`/`classify` (in `protocol/`, host-tested) +
   `CreatedBacking`; `BackingSize`; `WindowAllocator`; `RetireDomain`; `MappingTable::insert_unique`.
   **New counters, all must read 0 or be absent:** `VnEncOvf VnRingFt VnRingWd VnRingSz VnMtDown
   CpNoDrn PBTdErr CtNotOurs WtTbl AbnDrop ChSzMm ChSzDl ChSzPv MapDup PciCapOob WnRcf`.
   CollectDbgInfo is **version 6**, `[u32; 38]`, with `FENCE_WAIT_TABLE_FULL` appended at index 37
   (word 25 still `FENCE_WAIT_TIMEOUTS` — the report is decoded by index, never renumber).
   ⚠ **Stack budget, measured before and after with the new `tools/kmd-frame-sizes.ps1`:**
   `DxgkDdiStartDevice` 8376→**8408**, `VirtioGpu::init` 9112→**9160**, binding chain
   17488→**17568** of the 17936-byte known-good ceiling — **368 bytes of headroom left, down from
   448**. The script now reads sub-page frames, sums declared call CHAINS rather than every symbol,
   and exits 1 over the ceiling; run it on every image. The venus bring-up is the shallower chain
   at 12816 because R608 split it into `#[inline(never)]` per-stage frames.
   ⚠ **R614 (the `PassiveLevel` proof token) is DEFERRED to its own tranche and its own KMD
   image — owner decision 2026-07-27 (initially declined, reversed the same day).** The review sized it at "33 signatures plus caller groups";
   measured against the tree it is ~190 sites, because `venus.rs` sits between the DDIs and
   `virtio::ctrl` and every one of its 95 `VenusClient`/`VenusRing` methods transitively reaches
   `ctrl::sleep_ms` through `ring_wait_until`. Full cost: 34 ctrl entry points + 95 venus methods
   + 64 external `ctrl::` call sites + 23 `with_venus_client` sites, for a pure type-level change
   that buys no behaviour. **Consequence to remember: the PASSIVE-only contract of `virtio::ctrl`
   stays what it always was — one prose comment at `ctrl.rs:19` over 34 public functions that
   sleep, wait on KEVENTs or allocate contiguous memory, with no runtime assertion.** Anything
   that later wants the guarantee should re-scope it from the venus layer outward — which is
   exactly what makes it tractable: `with_venus_client` is the only gateway to
   `&mut VenusClient`, so requiring the token there and storing one in `VenusRing` replaces ~95
   threaded parameters with one structural claim (~130 sites, not ~190). See REFACTOR_REVIEW.md
   §T4a R614, which carries that plus the finding that all 34 entry points really are
   PASSIVE-only (even the `_async` ones allocate contiguous memory).
   ⚠ Formatting: `cargo fmt` was NOT run — the crate was already unclean before T4a (`lib.rs` 49
   hunks, `ddi/mod.rs` 26, `virtio/mod.rs` 20, all untouched here) and `cargo fmt` clean is T8's
   gate criterion. T4a added ~5 hunks in files it edited.
7b. **R614 LANDED — its own tranche, its own KMD image (2026-07-27, KMD `22.22.186.0`,
   8 commits).** `virtio::ctrl`'s PASSIVE-only contract is now a signature: `crate::irql`'s
   zero-sized `PassiveLevel` (`Copy`, `!Send`, no safe constructor) is threaded through all 34
   entry points, the internal primitives (`ctrl_roundtrip`, `ctrl_roundtrip_ok`,
   `resource_map_blob_roundtrip`, `sleep_ms`, `wait_block`, `reap_parked`) and `DmaBuffer::new`.
   **What it buys, concretely:** the DIRQL half of `DxgkDdiSetVidPnSourceAddress` holds no token,
   so `program_vidpn_source → ctrl::set_scanout_blob` is now UNREACHABLE from it — a compile
   error instead of the shipped DISPATCH deadlock the item was written for.
   **Three implementation decisions that differ from the review text, all deliberate:**
   (a) **BY VALUE, not `&PassiveLevel`.** A reference to a ZST is a real 8-byte pointer per frame,
   and the token threads through the boot chain that had 368 bytes of headroom. Measured on the
   built image before and after: every frame BYTE-FOR-BYTE UNCHANGED — `DxgkDdiStartDevice` 8408,
   `VirtioGpu::init` 9160, chain **17568** of 17936, headroom still 368.
   (b) **A COUNTED refusal, not the review's `debug_assert!`.** `kmd_render`'s `[profile.dev]`
   does NOT disable debug-assertions and cargo-make defaults to that profile, so THE SHIPPED
   IMAGE HAS THEM ON: a failing `debug_assert!` inside `assume()` would be a live `KeBugCheck` in
   a DDI — exactly what R601 spent four commits removing, and `verify-no-panics` greps only
   `.unwrap()`/`.expect(` so it would not have caught it. `assume()` counts into a packed
   `(count << 8) | last_irql` atomic mirrored as **`IrqlBad`** from `pacing_snapshot`, the same
   PASSIVE flush site `AbnDrop`/`WtTbl`/`WnRcf` use. It must read 0.
   ⚠ **This is PROVEN on the image, not inferred from the profile config**:
   `tools/kmd-debug-assert-check.ps1` finds the stringified expressions of all three existing
   `debug_assert!`s plus `assertion failed` inside the shipped 22.22.186.0 `.sys`, which a
   compiled-out assert could not leave behind. **FOUR `debug_assert!`s therefore ship today and
   are live bugcheck sites in DDI paths** — `virtio/ctrl.rs` (`reap_parked`), `virtio/gpu.rs` x2
   (`begin_parked_reap`), `ddi/present_packet.rs` (`debug_assert_eq!`). Deliberately NOT fixed
   here (R614's claim is "identical counters, identical desktop"); it wants its own item, either
   `debug-assertions = false` in `[profile.dev]` or four counted refusals. Note the comment at
   `gpu.rs:2259` claims they are "absent from the release driver" — true of the release PROFILE,
   misleading about the image that actually ships.
   (c) **Re-scoped from the venus layer outward**, as T4a's note said to: `with_venus_client` was
   VERIFIED (not assumed) to be the only path to a `&mut VenusClient`, so it takes the token and
   `VenusRing` stores one from bring-up. That replaced ~95 threaded parameters with one field —
   ~130 sites instead of ~190. `ScanoutGuard` carries a token too, which let the two locked
   scanout bodies drop their `_lock` underscores.
   **Twelve audited mints, and that number is the deliverable** (`grep -rn 'PassiveLevel::assume()'
   kmd_render/src/`): Escape, CreateAllocation, DestroyAllocation, Present, StartDevice,
   StopDevice, DestroyDevice (documented PASSIVE_LEVEL); BuildPagingBuffer, MapCpuHostAperture,
   UnmapCpuHostAperture, SetVidPnSourceAddress (**below a runtime `KeGetCurrentIrql` gate that
   already existed** — these four are checked, not asserted); and `hpd_thread_routine`
   (a `PsCreateSystemThread` body, the one structural justification).
   `DxgkDdiOpenAllocation` deliberately gets NO mint: it reaches no `ctrl::` entry point, and a
   mint with no consumer is laundering.
   **Gate evidence, all same boot (15:37:09) on 22.22.186.0:** cold boot to `CM_PROB_NONE`; a
   visible composited desktop (`helios_paintcap`) before AND after a `pnputil /restart-device`
   cycle; `IrqlBad` PRESENT and 0 at boot, mid-Fire-Strike, after two complete Fire Strike runs
   and across the restart-device — present rather than absent matters, because absence would be no
   evidence at all. `ASYNC_SUBMIT_COUNT == ASYNC_COMPLETE_COUNT` exactly at both 230902 (pre-restart
   generation) and 223100 (post-restart). Every failure counter clear, every T4a counter absent.
   Per-flip diag identical: `ScSet=1 ScFlu=3 VpDSt=0 DspMd=124257286 ScCpy=2 ScPch=7680`,
   `ScanoutDiag` absent. (`VpSA` is NOT a constant — it is a SAMPLED count written at n==1 and
   every 600th, so 1 at a fresh generation and 3000 under load are the same code. Do not gate on
   it.) `QfRet` 101-129/run, `AbnDrop` 0 on the clean generation. Host log same-boot, 
   `OPTIMAL DMA-BUF ready 1896x1030`, no venus decode/validation lines; the periodic
   `required=8773632 fd_size=7913472` shape mismatch is the pre-existing 38th-session class.
   **Measured:** Fire Strike Graphics **20144** (n=1) vs T4a's 19460/20312/20150 (n=3) — inside
   that cluster, so R614 moved nothing, and it makes a fourth sample for the re-baseline T4a asked
   for (four now at 19460/20144/20150/20312 against ONE pre-T4a sample at 21024, which is the
   weak side of that comparison). Physics 35443, Combined 5305. DComp 25 s runs
   1317/1350 pre-image → 1223/1319 post: inside the documented boot-to-boot spread, and this
   tranche changed no mechanism, so read nothing into it.
   ⚠ **NOT performed:** DOOM, rapid cursor motion (needs an interactive mouse — now FIVE tranches
   overdue), and the `HELIOS_VKR_DEBUG=validate` host capture (owner-owned launcher relaunch).
   **Suspend/resume is NOT TESTABLE as configured** and that is now a known fact rather than an
   omission: `powercfg /a` reports S1/S2/S3/Hibernate/S0-idle/Hybrid/Fast-Startup ALL unsupported,
   because `tools/launch-helios-gtk.sh` passes `-global ICH9-LPC.disable_s3=1` and
   `disable_s4=1`. Testing it needs that launcher change plus a full VM restart — owner-owned.
   **The honest limit, stated in `ctrl.rs`'s module doc and `irql.rs`:** the token does not prove
   the live IRQL — only `KeGetCurrentIrql` can, and a per-call check was out of scope. It proves
   PROVENANCE. One PASSIVE-only operation stays outside the type system: `DmaBuffer`'s `Drop`
   (`MmFreeContiguousMemory`), because `Drop::drop` has a fixed signature — which is why the
   transport parks completed buffers for `reap_parked` instead of letting the DISPATCH drain
   free one.
7c. **T4b LANDED — gate PASSED (2026-07-27, KMD `22.22.187.0`, 16 commits).** 18 of the 22 listed
   items implemented; **R716 and R717 were DEAD on arrival** — both live entirely inside
   `ddi/gdi_blit.rs`, which T1b deleted under the owner's GDI directive (R903 pulled forward), so
   the tranche is 20 items and all 20 are closed. **R904 (T6) landed FIRST**, as its cross-tranche
   note requires: R704's segment typestate cannot express the topo-11 shape, so encoding the
   must-be-last rule before that deletion would have changed behaviour rather than preserved it.
   **The one real bug fixed here is R702, and it was LIVE, not latent.** `query_driver_caps` formed
   `&mut *(pOutputData as *mut DXGK_DRIVERCAPS)` over a buffer the size gate deliberately permits
   to be shorter than the 592-byte struct — UB independent of which fields are touched, plus a
   Stacked-Borrows violation from writing through a second raw pointer while that reference was
   live. The review left open whether 24H2 actually passes a short buffer; **it does**: the
   `0x01CF` breadcrumb reads `0x240` = **576 bytes**, so the driver has been constructing that
   reference on every AddAdapter. Now every field is written through a bounds-checked
   `VersionedOut::set`; `CapTrunc` counts any field that will not fit.
   **Gate evidence, all same boot (17:32:13) on 22.22.187.0:** cold boot to `CM_PROB_NONE` AND
   `CM_PROB_NONE` after `pnputil /restart-device`, with a `Microsoft-Windows-DxgKrnl`
   all-keywords ETW trace over the restart — **74,706 events, 0 lost, NO `AzureTriage` record**
   (a real negative, not an empty capture). `kmd-gate-surface.ps1` → `GATE SURFACE CLEAN`.
   Per-flip diag identical to the T4a/R614 baseline: `ScSet=1 ScFlu=3 VpDSt=0 DspMd=124257286
   ScCpy=2 ScPch=7680`, `ScanoutDiag` absent, `AsSub == AsDone` exactly (1/1 idle, 58973/58973 and
   87455/87455 under Fire Strike), `WtOut`/`CtOut` 0. Knobs byte-identical: `BarM=10 BarF=0x1C
   BarB=0`. **Every new counter present and 0** — `CapTrunc SegRule SegDiv SegCntMis BarMCo EscHwA
   EscNoSy ApMiss` — plus `IrqlBad` present and 0, and `OaBadH` absent (never fired). Visible
   composited desktop via `helios_paintcap` after cold boot and after restart-device: full Win11
   shell — maximized Notepad with menus/status bar, layered console, live taskbar with clock.
   Host log same boot: `OPTIMAL DMA-BUF ready 1896x1030` twice, no venus decode/validation lines,
   no Xid; the periodic `required=8773632 fd_size=7913472` shape mismatch and the paired
   `glEGLImageTargetTexture2DOES failed: 0x502` are the pre-existing 38th-session class.
   **Declared value changes, all three verified:** `0x01D6` now carries the aperture commit limit
   in **MiB** (it was always `0x01D6_0000` because 64 MiB has no low 16 bits — useless for the one
   thing it reports); `root_page_table_size_bytes` returns a derived size for `num_pte > 512`
   (unreachable by the declared geometry); and R722's two colliding breadcrumbs moved —
   HPD-worker-create-failed `0x0B00_00E7` → **`0x0B00_00EA`** and ExchangePreStartInfo
   `0x0E00_0001/2` → **`0x0E10_0001/2`**. Both old and new values are recorded in `diag.rs`'s
   encoding header. `BRINGUP_QUIRKS.md:172` cites `0x0E00_0001` as DestroyDevice entry and stays
   correct; no ROADMAP recipe cited either.
   **Measured:** Fire Strike **Graphics 20473** (Physics 34663, Combined 5439, Overall 16850) —
   the highest of five samples against T4a's 19460/20312/20150 and R614's 20144, i.e. inside a
   ~5 % spread that is wider than any tranche's effect; read no improvement into it either.
   A full run completed with `AsSub == AsDone` at **227685/227685** and again at 250985/250985
   after two DComp probes — zero leaked submissions across ~250k submits. DComp 25 s runs
   **1308 / 1292** frames (52.3 / 51.7 fps), both `PROBE PASS`, inside 22.22.186.0's 1223/1319.
   `AbnDrop` 0 idle → 70 after the soak, which is VidSch preempting under load, not a failure.
   **Measurements this tranche added, and what they say:** `ApMiss` = 0 on the production
   `DisplayHalf=1` shape, exactly as R718 predicted. `BlbSzD` = **1** — the empirical linear-blob
   size guess (`NV_LINEAR_ROW_ALIGN` 128 + `NV_LINEAR_TAIL_SLACK` 64 KiB) disagreed with the exact
   Vulkan requirement once this boot; that is the first quantification of how good that guess is.
   **`PgSm` = 0 through a full Fire Strike run — the Present-time system-backing mirror NEVER
   FIRES.** That is the deciding evidence R715 commit 2 was gated on: deleting the mirror is
   cheaper than building a VidMm page lease for it. `EscHwA` = 0 likewise unblocks R706 sub-commit
   (2) for a future image.
   **Four scope deviations, all deliberate and all in the commit messages:** (a) `KnobName`
   covers every knob READ (14 sites, now compile-checked at `cargo check` rather than only at
   monomorphisation) plus a `const _: () = assert!` sweep over `FaultCounter::ALL`, but NOT the 478
   `record_named_bytes` call sites; (b) R706 sub-commit (2) is count-only by owner decision, since
   its evidence can only come from this image; (c) R715 commit 2 is owner-gated and only commit 1
   landed — no newtype with a no-op `Drop`, which the review calls cosmetic; (d) R722's
   `diag::codes` migration across ~20 files is skipped — its guarantee is weak by the review's own
   admission and the defect it prevents is fixed directly here.
   **One removed KeBugCheck site:** `write_patch_references`'s `debug_assert_eq!` was one of the
   four `debug_assert!`s that SHIP in this image; `PatchCapacity` now carries the count that made
   it structurally impossible. Three remain, all in the virtio layer.
   **Stack:** deepest chain 17584 B (ceiling 17936, headroom 352), +16 B against T4a's 17568 —
   `AdapterKnobs` and `SegmentTable` are new StartDevice locals and the optimizer folded all but
   16 bytes.
   ⚠ **NOT performed:** DOOM, rapid cursor motion (needs an interactive mouse — now SIX tranches
   overdue), the `HELIOS_VKR_DEBUG=validate` host capture (owner-owned launcher relaunch), and the
   `DiagLevel=1` S-ring diff of the `0x01D0`–`0x01D8` / `0x09*` records, which needs a SECOND
   reboot because `DIAG_LEVEL` is cached at driver load and `restart-device` does not reload the
   image. The caps and segment values were instead verified against the values the counters and
   the AddAdapter success path expose. **Suspend/resume remains NOT TESTABLE** for the reason
   recorded under 7b (`disable_s3=1`/`disable_s4=1` in `tools/launch-helios-gtk.sh`).
   ⚠ **WS1 defect 0z reproduced unchanged and is NOT this tranche's:** 5 application faults this
   boot, all `vulkan_virtio-*.dll` `0xc0000005`, all in dwm/Explorer/SearchHost/
   StartMenuExperienceHost — every one already in the historical set of 9 processes, and the only
   exception code ever seen. 1167 such faults now in the log (889 three weeks ago). NO non-ICD
   application error this boot. `tools/icd-fault-history.ps1` is the A/B that establishes that.
7d. **T5 PARTIALLY LANDED — 23 of 30 items COMPLETE, 7 have unfinished halves, GATE NOT RUN
   (release UMD only; no KMD image, no reboot).** 32 commits on `wddm`, base `aff882f`.
   ⚠ An earlier version of this entry said "all 30 items closed". **That was wrong** — every
   one of the 30 was touched and committed, but seven landed with a deferred half and one used
   a different mechanism than the review specifies. The precise remaining work is in **7e**.
   Note `REFACTOR_REVIEW.md`'s summary table at line 178 says 29 and is **wrong**.
   Every commit builds clean and the crate warning count is unchanged from the pre-tranche
   baseline (16) at every step.
   **The one real BUG with a live wrong value was R801:** two constants shared the name
   `DXGI_ERROR_UNSUPPORTED` with different values, and `OpenAdapter12` returned `0x887A_0020`
   = `DXGI_ERROR_DRIVER_INTERNAL_ERROR`, so a D3D12 client's ordinary unsupported-DDI
   negotiation was recorded by the runtime and by ETW as a **driver fault**. Both printed as
   "DXGI_ERROR_UNSUPPORTED" in our own log, so the divergence was invisible to a triage grep.
   **Two owner decisions, taken up front so they could not block the other 28:** R829 →
   *correct the doc* (`helios_multisample_quality_levels` keeps advertising 8x for every
   output-capable format; D3D11.3 §19.2.5 is a FLOOR, not a ceiling, and the caps/quality pair
   stays coherent because `check_format_support` shares the predicate). R830 → *name the
   literals only*, values UNCHANGED, with the cap reduction DEFERRED pending same-boot evidence
   on whether DWM queries MPO at all.
   **Machine-checked ABI is the tranche's biggest single gain.** `layout_tests(true)` in
   `umd/build.rs` gives **6,336 assertions across 818 types** (817 size, 815 alignment, 4704
   field offsets), and — contrary to R802, which predicts unrunnable `#[test]` functions —
   bindgen 0.70 emits them as `const _: () = { ["msg"][offset_of!(X,y) - N]; }`, i.e.
   **compile-time**. Verified by corrupting `D3D10DDIARG_CREATEDEVICE`'s `hDrvDevice` offset
   32→24 (exactly R802's named invalid sequence) and confirming `error[E0080]`. The seven
   hand-transcribed d3d10umddi structs are deleted; the five `D3d12Ddi*` ones stay because
   `build.rs` bindgens `d3d10umddi.h` only.
   **Three static guarantees were verified with throwaway negative controls, not asserted:**
   `load_com::<ID3D11RenderTargetView>(h_rtv)` → `the trait bound
   D3D10DDI_HRENDERTARGETVIEW: ComHandle is not satisfied` (R803);
   `dev.dxvk.d3d11_device_ptr()` → `no method named d3d11_device_ptr found for struct
   BridgeDevice` (R815); and transposing `present_vehicle_copy`'s two pointers by type →
   `error[E0308]` (R816). R814 was validated by hashing the generated `bridge.rs.cc` before and
   after — **byte-identical**, so the `unsafe` re-marking provably changed no codegen.
   `forward.rs` now contains **zero raw handle-slot casts** (was 14 across five spellings); all
   live in the new `forward/handles.rs`, which is T8's stated precondition.
   **DECLARED BEHAVIOUR CHANGES, four, all counted:**
   (a) **R801** — `OpenAdapter12` now returns `DXGI_ERROR_UNSUPPORTED` (`0x887A_0004`).
   (b) **R806 sub-commit 2** — `create_resource` refuses a scan-out-primary create whose bridge
   resource has a **zero row pitch**, releasing it and reporting `E_OUTOFMEMORY`, instead of
   stamping `MISC_PRIMARY | MISC_DIRECT_SCANOUT` into the KMD meta for an allocation the UMD
   never registers in `direct_scanout_allocations`. New counter `SCANOUT_PRIMARY_ZERO_PITCH`.
   **Expected 0** — the review calls this state "constructible today" and it is NOT:
   `create_ddi_scanout_texture2d` returns 0 for a zero width/height, hard-wires `out_offset` to
   0 on every path, and otherwise computes a non-zero pitch, so `raw != 0` implies `rp != 0`.
   This closes a cross-FFI contract dependency, not a live bug.
   (c) **R812** — `pfnCheckDeferredContextHandleSizes` is a VOID writer, not a size getter; it
   had TWO different stubs across the four device-funcs tables (the 256-returning `calc!` stub
   in three, `ddi_noop_device` in the 11.0 table, whose list omits it). All four now install one
   typed stub that writes 0 to the count out-param. Writing nothing, which both old stubs did,
   left the runtime reading whatever it had pre-set.
   (d) **R817** — `present_flip_wait_setup` returns **false** with a counter when called with
   parameters differing from the armed ctx, where it previously returned **true** while
   discarding them. Unreachable today; it becomes reachable after a device reset with a new
   monitored fence, where the old behaviour left the watchdog dereferencing a retired
   monitored-fence mapping.
   **New UMD counters, all process-global `AtomicUsize` (the UMD still has no registry counter
   surface):** `SCANOUT_PRIMARY_ZERO_PITCH`, `SCANOUT_DIRECT_OVER_LINEAR`,
   `SCANOUT_DOWNRES_KEPT`, `SCANOUT_ZERO_EXTENT`, `ADAPTER_UNRECOGNISED`,
   `CHECK_DEFERRED_HANDLE_SIZES_CALLS`; C++ side `flipWaitSetupMismatch`,
   `flipWaitSetupConcurrent`, `s_publishTableFull`, `s_gateExceptions`, `s_gateNoContext`, the
   scan-out format refusal. The `present-gate:` line KEEPS its existing keys and APPENDS
   `failed=` and `noctx=`.
   **Four items landed structure-only, with the behaviour half deferred and INSTRUMENTED so the
   gate produces the evidence to decide it:** R809 (the DirectPrimary-wins rule and the
   down-resolution policy — this is the frozen direct-primary display path and the failure mode
   is a blank desktop; `SCANOUT_DIRECT_OVER_LINEAR` / `SCANOUT_DOWNRES_KEPT` / `SCANOUT_ZERO_EXTENT`
   measure whether they ever fire), R818 sub-commit 2 (the Rust trampoline — its stated benefit
   is already delivered by asserting the CPU-signal ABI's 24/0/8/16 on BOTH sides, and it would
   run on DXVK's fence-waiter and watchdog threads), R820 sub-commit 2 (the move to
   `format_caps.rs` — the const-asserts that make it safe now exist, and T8 is the move tranche),
   and R803's resource/RTV signature conversion (~110 further call sites in a file T8 splits,
   closing no hazard those decoders have).
   **Corrections to `REFACTOR_REVIEW.md` found while implementing, worth carrying into T6–T8:**
   R802's "seven structs" counts what is REPLACEABLE, not what exists (twelve were
   hand-written); `E_FAIL` was defined FOUR times, not three; `_pad` had SIX kernel read sites,
   not four, and the cited line numbers were stale; there are FIVE `store_resource` call sites,
   not four; R809's prescribed `Cell<Option<ScanoutTarget>>` cannot compile because the variant
   owns an `ID3D11Resource`; and R816's cxx **shared structs** are not constructible here at all
   — `dxvk_bridge.h` is included BY the cxx-generated glue, so a shared struct is declared after
   it and cannot appear in `HeliosDxvkDevice`'s signatures. R816 uses Rust-side newtypes
   instead, which R815 makes airtight for every reachable caller.
   **Two dead fields surfaced by the R809 grouping:** `allocation` and `generation` each had
   writers and no reader — invisible while they were loose `Cell`s. Both kept (scan-out identity
   the KMD matches on) and now logged from stored state; deletion is T6's call.
   **New tooling:** `tools/umd-check.ps1` (filters the UMD build on the VM — a full build emits
   ~115 clang warnings from the vendored dxvk-helios headers, enough to blow the MCP output cap
   and drop the rustc errors, which happened twice) and `tools/helios_ownership_soak.cpp` +
   `tools/helios-ownership-soak.ps1` (the gate's headline ownership run, which did not exist).
   ⚠ **GATE NOT YET RUN.** Nothing here has been deployed or measured on the guest. The release
   UMD must be built and installed (`win_install_umd` with an explicit `umd_dll` — Defender
   blocks the debug copy and a stale debug DLL silently invalidates every timing number), then:
   the ownership soak; a `UmdTrace=1` ABI diff against a pre-change run; visible desktop, Fire
   Strike, the in-tree D3D11 probe suites as the dxvk-tests surrogate (dxvk-tests is NOT
   installed on this box), DComp cadence, idle-to-active wake, cursor with no trails; the
   standing KMD surface (`kmd-gate-surface.ps1`, per-flip diag `ScSet=1 ScFlu=3 VpDSt=0
   DspMd=124257286 ScCpy=2 ScPch=7680` with `ScanoutDiag` absent, `AsSub == AsDone`, `WtOut`/
   `CtOut` 0); and `helios_paintcap` → `Z:\tmp\screen_copy.png` as the only rendering evidence.
   ⚠⚠ **NEW DEFECT FOUND BY THE SOAK HARNESS — PRE-EXISTING, NOT T5's. WS1 (stability).**
   **`D3D11CreateDevice` / `Release` on the Helios adapter leaks exactly 6 kernel handles and
   ~135 KiB per device, linearly, with no plateau.** Measured 2026-07-27 against the DEPLOYED
   **T4b release UMD** (the DriverStore copy the runtime actually resolves —
   5,917,696 B, 27-07 00:33; note `C:\ProgramData\HeliosUmd` still holds a stale 03-07 build
   that nothing loads), so this is a BEFORE number and predates the tranche:
   ```
   baseline (post-warmup)  handles=148    modules=24   ws=10648 KiB
   device cycle 100        handles=746    modules=24   ws=24092 KiB
   device cycle 200        handles=1346   modules=24   ws=37676 KiB
   device cycle 300        handles=1946   modules=24   ws=50908 KiB
   ```
   +598/+600/+600 per 100 cycles = **6.00 handles per device**, dead linear.
   ⚠⚠ **CORRECTION (2026-07-27, later the same day): that run did not COMPLETE — it CRASHED,
   and "300 cycles" was not a chosen scale but the last sample before the crash.** The harness
   defaults to 1000 device cycles and had never actually been run at its own default. At that
   scale it dies **between device cycle 301 and 400, deterministically** (three runs, identical
   to the sample), with **exit `0xC0000409` (STATUS_STACK_BUFFER_OVERRUN / fail-fast), faulting
   module `ucrtbase.dll`, fault offset `0xa527e`, WER bucket type BEX64**. The first attempt
   produced *no output at all* because the harness's stdout was block-buffered into a pipe and
   the fail-fast discarded it — fixed in `ff16107` (`setvbuf(_IONBF)`), which is what made the
   crash window visible.
   **The WARP control isolates BOTH the leak and the crash to our UMD**: identical cycling
   against "Microsoft Basic Render Driver" gives **handles 137 → 137 → 137 → 137**, `+0 over
   1000 cycles`, `OWNERSHIP SOAK PASS`, **no crash**. So neither is a D3D11/DXGI property of
   this box.
   ★ **This escalates the leak from "unbounded growth" to "the process dies after ~350
   devices"** — a hard failure mode, not just resource pressure. Handle count at the crash is
   only ~2500, nowhere near any system limit, so handle exhaustion is NOT the mechanism and
   the cause is something else that grows per device.
   **A full WER dump is captured for whoever root-causes it:**
   `C:\Users\Rupansh\helios-probe\dumps\helios_ownership_soak.exe.3352.dmp` (150 MB, DumpType 2;
   LocalDumps registered under
   `HKLM\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps\helios_ownership_soak.exe`).
   Note the probe is built without `/Zi`, so it has no PDB — symbolising it needs a rebuild
   with debug info first. Deliberately NOT guessed at beyond this.
   **The resource phase is FLAT on Helios** — handles 1977 at cycle 1000, 2000 and 3000 — so
   the paths T5 restructured (`Slot<Boxed<ResourceState>>`, `RtvState`, `DeallocateForm`, the
   R806 descriptors) do not leak. The leak is entirely in device teardown.
   Not yet root-caused, and deliberately not guessed at: candidates are DXVK's per-device
   threads, the WS1 #4 named present fence, and the `pfnCreateSynchronizationObject2Cb`
   monitored fence R810 touched (which has no destroy path — though that one is inert at
   `PresentSyncPublish=0` and so cannot be this).
   ⚠ **Consequence for the T5 gate, in two parts:**
   (a) its ownership criterion as written ("handle count FLAT") **cannot pass on this
   codebase**, because the baseline already fails it. The honest gate is *no worse than 6.00
   handles/device and a flat resource phase*; the absolute leak is a separate WS1 defect. The
   harness prints the handles-per-device-cycle figure directly so before/after compare on one
   number, and its working-set tolerance is calibrated to the WARP control's own ~8 MiB of
   runtime noise rather than to zero.
   (b) **its SCALE cannot be met either.** "1000 CreateDevice/DestroyDevice cycles" is
   unreachable on any build, before or after, because of the crash above. The matched pair is
   therefore run at **300 devices / 10000 resources** — the largest device count that survives —
   and the gate reads the per-device figure plus a flat resource phase. A future tranche that
   fixes the crash should re-run at the specified 1000/10000 and record it here.
7e. **T5 REMAINING WORK — CLOSED. Two items implemented, six dispositioned.**
   Supersedes the "seven unfinished halves" list this entry used to carry. Of the eight open
   points, **two were the real work and are now landed** (R803's boxed half, R802 sub-commit 3);
   **five are deferred with a named destination and a stated reason**; **one (R813) is closed as
   rejected**. Nothing in T5 is now blocked, and `u-forward-a-01` (T8's `forward.rs` split) has
   its stated precondition.

   **(1) R803 sub-commits 2b..f — the boxed-payload half. LANDED.** Five commits, readers before
   owners so the only two functions per family that can double-free were reviewed alone:
   - *2b* `BoxedHandle` + `boxed_slot()` in `forward/handles.rs`. An associated `State` type pairs
     each boxed handle with the one struct its slot holds (`HRESOURCE`→`ResourceState`,
     `HRENDERTARGETVIEW`→`RtvState`, `HELEMENTLAYOUT`→`LayoutData`), so the payload is derived
     from the handle's type instead of chosen at the call site. Element-layout family converted
     (3 sites) as the infrastructure's first user. Both are `pub(super)`, not `pub(crate)`: the
     payload structs are private to `forward` and a `pub(crate)` trait may not leak them (E0446).
   - *2c* the ten resource readers + `presented_primary_private`, 87 sites. Also re-typed
     `rtv_desc`/`dsv_desc`/`srv_desc`, which took a `resource_priv: *mut c_void` purely to reach
     `resource_sample_count`.
   - *2d* `store_resource`/`release_resource` (6 sites) — the ownership-transfer pair, alone.
   - *2e* the three RTV readers (6 sites); *2f* `store_rtv`/`release_rtv` plus the last raw boxed
     decode, in `dxgi_rotate_resource_identities`.
   **`Slot::<Boxed<_>>::from_priv` now survives at exactly two sites**, `resource_state_at` and
   `rtv_state_at` — the runtime-tag forms for `Discard`, `ClearView` and the tiled-resource
   barrier, where a `D3D11DDI_HANDLETYPE` picks the payload at *run* time and no static type
   exists to key on. `ClearView`'s `hView` is a bare `*mut c_void` in the DDI itself, so there is
   no handle to pass even in principle. These sit beside the pre-existing `handle_com_raw_at` /
   `load_com_at` and carry the same contract.
   `forward.rs` still names `pDrvPrivate` 66 times; **none of them decode a boxed slot** — they
   are the `HDEVICE` private-memory cast (a different payload kind), `{:p}` log fields, null tests
   on bare-COM handles, and the selftest's handle literals.
   Verified with throwaway negative controls, not asserted: `resource_state(h_rtv)` →
   `error[E0308]: mismatched types: expected D3D10DDI_HRESOURCE`, and
   `let _: Option<Slot<Boxed<ResourceState>>> = boxed_slot(h_el)` → the same. Both removed.

   **(2) R802 sub-commit 3 — the const-asserts in `ddi.rs`. LANDED.** `size_of` == 88 plus the
   eleven `D3D10DDIARG_CREATEDEVICE` offsets, `DXGI_DDI_BASE_ARGS` == 16, `OPENADAPTER` == 40,
   `GETCAPS` == 32. **The reason given for skipping this in `c090fdf` was wrong, and that commit
   message should not be trusted on the point:** bindgen derives both the struct and its 6,336
   layout assertions from the same header, so they are self-consistent by construction and a WDK
   revision that moved a field would regenerate both and pass. What they catch is bindgen
   disagreeing with the C compiler, not the ABI moving under us.
   The accurate justification is narrower but real: `create_device`'s `CreateDevice raw args:`
   dump walks the argument as `*const u64` and prints words 0..=10 **by index**, bounded at 11
   words on the premise that the struct is 88 bytes. Values lifted from `c090fdf`'s
   `abi_equivalence_proof` module (deleted in `2d1fdb0`), so they are the same numbers that were
   checked against the hand-transcribed struct before it was removed.
   Confirmed live: flipping `hDrvDevice` 32→24 fails with `error[E0080]` at `ddi.rs:60`.
   ★ **Independently confirmed by the gate's own ABI capture**, which is stronger evidence than
   the compile-time check: the `UmdTrace=1` dump shows `[2]`=pKTCallbacks, `[3]`=pDeviceFuncs,
   `[4]`=hDrvDevice, `[6]`=pDXGIBaseFuncs, `[8]`=pUMCallbacks, `[9]`=Flags — i.e. the runtime
   really does place those fields at bytes 16/24/32/48/64/72, exactly as asserted.

   **(3) R809 — the two behaviour changes. DEFERRED, INSTRUMENTED, DECISION PENDING.**
   *Done:* one `RefCell<Option<ScanoutTarget>>` with a sealed `ScanoutKind`; the third writer
   can no longer leave the import stale; `publish_dwm_composition` matches `KmdLinearImport`
   explicitly. *Missing:* the **DirectPrimary-wins rule** (a LINEAR import may not displace an
   exact primary) and the **down-resolution policy** (today "largest area wins" silently keeps
   older, larger geometry). Both are behaviour changes on the **frozen direct-primary display
   path** whose failure mode is a blank desktop, which is why they were instrumented instead:
   read `SCANOUT_DIRECT_OVER_LINEAR`, `SCANOUT_DOWNRES_KEPT` and `SCANOUT_ZERO_EXTENT` after a
   gate run with an up **and** down resolution change. **If all three read 0, the two rules are
   unreachable and can be adopted safely or dropped as dead.** That is the whole point of the
   counters — do not adopt the rules without that reading.
   ⚠⚠ **The counters had NO READER until `9256a6e`, so that procedure was not executable
   against the code as it was committed.** Three of the four were process-global atomics that
   nothing ever loaded — they incremented into a void. `scanout_counter_summary()` now appends
   `direct_over_linear= downres_kept= zero_extent= zero_pitch=` at every decision point on both
   scan-out paths. Note the `downres_kept` branch is an **early return**: a summary only on the
   success path would have been blind to the single policy it most needs to measure, which is
   how the gap survived. **Read the values from the DWM UMD log** (`C:\ProgramData\Helios\
   umd-<dwm pid>.log`, grep `DDI scanout target`), not from the registry — the UMD has no
   registry counter surface.

   **(4) R812 — the paired calc/create descriptor. DEFERRED to T7 `u-core-07`.**
   *Landed in T5:* `pfnCheckDeferredContextHandleSizes` reclassified out of the `calc!` lists into
   one typed void stub in all four tables, plus `const _: () = assert!(THREADING_CAPS == 0)` tying
   the three remaining 256-byte stubs to the cap. *Not done:* the `DdiObject`/`NoDriverPrivate`
   descriptor that would make "a real Create with a stub size" unrepresentable for the marked
   classes. It needs the paired **Create** slot names, which the blanket noop pass fills and
   nothing names; enumerating them here would be exactly the second table-fill abstraction the
   review forbids building in T5. `u-core-07` rewrites all four fills anyway — the descriptor is
   nearly free there and is a duplicated table here.

   **(5) R813 — the eight shader-create wrappers. CLOSED AS REJECTED (cosmetic).**
   Audited rather than implemented, and the audit stands under the review's own standard: all
   **ten** call sites already guard the bridge's 0 sentinel with `if raw != 0` before
   `store_raw_com`; storing a zero would be harmless anyway because `load_com` null-checks the
   slot; and the result goes into the slot as a raw word rather than being wrapped
   owned-or-borrowed, so there is no wrong-adoption hazard of the kind `Slot<P>` exists to close.
   An `Option<usize>`/NonZero wrapper here would restate a check already present at every site and
   close nothing. If the shader creates are consolidated in T7 the wrapper comes free with that;
   it is not worth its own commit.

   **(6) R818 sub-commit 2 — the Rust trampoline. DEFERRED, no destination claimed.**
   *Landed in T5:* the CPU-signal ABI's 24/0/8/16 is static_asserted in C++ **and** const-asserted
   against the bindgen type in Rust, so a WDK change fails the build. **That is the whole of the
   item's stated benefit.** *Not done:* exporting `helios_flip_signal` and repointing
   `present_flip_wait_setup`'s first parameter at it. What that buys is removing a duplicate
   *declaration*; what it costs is a Rust trampoline running on DXVK's fence-waiter and watchdog
   threads, which must not allocate, log, or touch any `Cell`/`RefCell` device state. Bad trade on
   its own — fold it into whichever tranche does other trampoline work.

   **(7) R820 sub-commit 2 — move to `forward/format_caps.rs` + the pipeline. DEFERRED to T8.**
   *Landed in T5:* six previously-unnamed `D3D11_FORMAT_SUPPORT` bits, all five WARP values
   expressed as compositions and pinned by `const _: () = assert!` to the hex they replace, bare
   integers replaced by named `DXGI_FORMAT_*` constants. *Not done:* the file move, the explicit
   `base -> scrub_video -> warp_family_override -> reassert_msaa` pipeline, and reading
   `feature_level_mode()` once instead of four times. **The const-asserts that make the move safe
   now exist**, so this is unblocked and purely a question of which tranche moves the file. T8 is
   the move tranche and will relocate this file's contents regardless.

   **(8) R816 — MECHANISM DEVIATION, gap acknowledged, closure is T7/T8-sized.**
   The review specifies cxx **shared structs** so the guarantee holds in BOTH languages. That is
   structurally impossible here: `dxvk_bridge.h` is included **by** the cxx-generated glue and
   `dxvk_bridge.cpp` includes no generated header, so a shared struct is declared after
   `HeliosDxvkDevice` and can never appear in its signatures. The review's own `PresentPair`
   fallback has the identical problem. Rust-side `SrcRes`/`DstRes` newtypes were used instead,
   which R815's sealing makes airtight for **every reachable Rust caller** (transposing them is
   `error[E0308]`). **The residual gap: a future edit to `dxvk_bridge.cpp` that transposes the two
   parameters on the C++ side is still unguarded.** Closing it properly means restructuring the
   pimpl/header arrangement so the bridge can consume cxx-generated types — out of scope for a
   tranche that is not allowed to move files.

7f. **T5 GATE RUN AND PASSED (2026-07-28), UMD `0daa30ca3ac146ec`, KMD unchanged at
   22.22.187.0.** UMD-only tranche, no KMD image, no reboot; three `pnputil /restart-device`
   cycles. T4b backup intact at `C:\Users\Rupansh\helios-umd-backup-t4b.dll`
   (`F0C7A2E6…25CDFD`) and is what every BEFORE number below was measured against.
   ⚠ **Deploy trap, hit once:** `umd-check.ps1` builds into the MIRROR
   (`C:\Users\Rupansh\helios-vgpu\umd\target\release`), and `Z:\umd\target\release\` does not
   exist at all. The first deploy shipped a release DLL built BEFORE the last commit; caught
   because `direct_over_linear=` was absent from the binary's string table. **Always pass
   `umd_dll` explicitly and check a string you just added is in the shipped image.**

   **Ownership soak — the headline. PASS on the honest criterion, and the numbers are
   IDENTICAL before and after.** 300 devices / 10000 resources (see 7d(b) for why not 1000):
   ```
                         T4b BEFORE                T5 AFTER
   baseline              148                       148
   device 100/200/300    746 / 1346 / 1946         746 / 1346 / 1946
   per-device            5.99 handles              5.99 handles
   resource 1000..10000  1977, all ten samples     1977, all ten samples
   final / modules       1952 / +0                 1952 / +0
   failures              0 device, 0 resource      0 device, 0 resource
   working set delta     +38548 KiB                +38644 KiB   (0.25% apart)
   ```
   Every handle count matches exactly. **The resource phase — the paths T5 actually
   restructured (`Slot<Boxed<ResourceState>>`, `RtvState`, the R803 accessor conversion,
   `DeallocateForm`, the R806 descriptors) — is perfectly flat across 10,000 cycles.** The
   `OWNERSHIP SOAK FAIL` verdict the harness prints is the pre-existing 6-handles-per-device
   device-teardown leak failing the literal "flat" test, identically on both sides.

   **ABI — IDENTICAL.** `UmdTrace=1` capture on T4b vs T5, compared structurally by the new
   `tools/abi-diff.ps1` (a text diff is pure noise — the dump prints heap pointers). Word count
   11, `word[1]`/`word[9]`/interface/version/flags byte-identical, and **every named field
   resolves to the same word index**: pKTCallbacks→2, pDeviceFuncs→3, hDrvDevice→4,
   pDXGIBaseFuncs→6, pUMCallbacks→8, Flags→9. That is bytes 16/24/32/48/64/72 — independent
   live confirmation of R802 sub-commit 3's const-asserts.

   **Rendering evidence (the only kind that counts):** `helios_paintcap` →
   `Z:\tmp\screen_copy.png` twice, once right after deploy and once after a device restart plus
   60 s idle. Full composited desktop both times — wallpaper, taskbar, icons, window frames,
   clock matching the capture minute. The second capture also caught a window mid-fade, which
   is live compositing rather than a stale surface.

   **Performance, all within or above the T4b reference:**
   - DComp cadence **1316 / 1311** frames per 25 s (52.6 / 52.4 fps), both `PROBE PASS`, vs
     T4b's 1308 / 1292. ⚠ An earlier pair read 1196 / 1161 and that was **entirely a
     `UmdTrace=1` confound** left set from the ABI capture — the knob is read once per process
     at device init, so dwm had tracing on. **Clear `HKLM\SOFTWARE\Helios` and restart the
     device before any cadence measurement.** Idle log growth with the knob absent: 0 bytes/min.
   - Fire Strike **Graphics 20212** (GT1 141.1 fps, GT2 63.8, Physics 33553, Combined 5507),
     inside T4b's own five-sample spread of 19460–20473.
   - Idle-to-active wake: desktop composited correctly after 60 s idle; 1.1 s from schtasks
     trigger to PNG on disk, which is task launch + process start + capture + write, so the
     wake component is well under that.

   **Conformance:** both in-tree D3D11 probe suites (`helios_d3d11_knob_suite`,
   `helios_d3d11_extra_suite`) pass end to end — every `rc=0`, `TOTAL failures=0` on the
   upload-integrity sweep across five sizes × three upload paths × two bind flags, and
   feature-level, cross-adapter, shared-resource, keyed-mutex, KMT-open and cross-process draw
   all `hr=0x00000000`. dxvk-tests is NOT installed on this box; these are the surrogate.

   **KMD surface CLEAN** (`kmd-gate-surface.ps1`): every must-be-zero counter clear including
   `WtOut`/`CtOut`/`IrqlBad`; per-flip `ScSet=1 ScFlu=3 VpDSt=0 DspMd=124257286 ScCpy=2
   ScPch=7680`, `ScanoutDiag` absent, **`AsSub == AsDone` (261177)**. `QfRet=117` and
   `AbnDrop=34` are backpressure/preemption, not failures, and are the right order for a Fire
   Strike plus two probe suites.

   **UMD surface CLEAN** (new `tools/umd-gate-surface.ps1`). The eleven new counters:
   `zero_pitch=0` (R806/2), `zero_extent=0`, `downres_kept=0`, `flip-kwait setup REFUSED`
   absent (R817), `CheckDeferredContextHandleSizes called` absent (R812), `adapter handle not
   ours` absent (R801), `NO SLOT published` absent (R826), and `present-gate: … failed=0
   noctx=0` on every T5 process. **`PresentSyncPublish=1` + two dcomp-vehicle runs were done
   specifically because R810/R817/R818 are inert at defaults** — still `failed=0 noctx=0`,
   cadence 1184 / 1330. Knob removed and the device restarted afterwards; the box is back at
   defaults.
   ★ **`SCANOUT_DIRECT_OVER_LINEAR` is NOT zero — 17 in one dwm session, 2 and 1 in others.**
   See 7e(3): this settles half of R809's pending decision. The DirectPrimary-vs-LinearImport
   competition the deferred rule governs **fires routinely on an ordinary desktop**, so that
   rule must NOT be dropped as dead code. `downres_kept` remains 0 but is **unexercised, not
   proven unreachable** — see the resolution-change limitation below.

   **WS1 defect 0z reproduced unchanged and is NOT this tranche's.** 1161 → 1165 ICD faults
   all-time across three device restarts; 17 this boot, all `0xc0000005`, all in
   dwm/Explorer/SearchHost/StartMenuExperienceHost — the historical process set and the only
   exception code ever seen. **Zero non-ICD application faults this boot.** A T5 regression
   would be a new process or a new exception code; there is neither.

   ⚠ **NOT performed, and why:**
   - **Rapid cursor motion / no trails** — needs an interactive mouse. Now EIGHT tranches
     overdue and still the only gate line no automated instrument covers.
   - **Resolution change up and down**, so `SCANOUT_DOWNRES_KEPT` stays unexercised.
     `tools/res_change_probe.ps1` was written for it and is committed working, but
     **`ChangeDisplaySettingsEx` is not an available mechanism on this box**: running as the
     console user in session 1 (verified, not assumed), `EnumDisplayDevicesW` returns FALSE at
     index 0 and `EnumDisplaySettingsW` fails, so there is nothing to act on — the same family
     as the historical "no DXGI output" symptom. **This also blocks WS1's resize soak**, which
     needs a working mode-change mechanism, and is worth its own item.
   - **Suspend/resume** — still NOT TESTABLE (`disable_s3=1`/`disable_s4=1`, see 7b).
   - **The soak at its specified 1000-device scale** — unreachable on any build, see 7d(b).

8. Implement the remaining reviewed refactors atomically, in tranche order, one recommendation
   per commit; never fold a `BUG` fix into a structure move. Preserve the current direct
   primary, completion ordering, loud-failure contracts, registry ABI, and
   diagnostic names unless a reviewed change explicitly migrates them. Replace
   arbitrary `Sleep`/poll loops with event, interrupt, fence, or
   condition-variable contracts; do not remove bounded safety timeouts merely
   because they wait.
9. Regression-test each tranche and the final driver against visible desktop
   output, idle wake, rapid cursor motion, DComp cadence, DWM stability,
   same-boot KMD breadcrumbs, and host scanout evidence. Keep
   `ScanoutDiag` absent during primary tests.
10. Continue soaking the current direct-primary path across DWM buffer rotation,
   resize, suspend/resume, device restart, and cold boot.
11. Pursue true host zero-copy only with a layout contract the display importer
   can consume. An explicit DRM modifier is one possible route, but enabling the
   modifier/DMA_BUF extensions on every DXVK device is prohibited: it inflated
   ordinary shared OPTIMAL import requirements and caused valid undersized-import
   refusal, DWM failures, and NVIDIA Xid 31 when bypassed.
12. Continue D3D11 stability and conformance work after the quality pass.

## Historical PSC workstreams

The dated IDD/Looking Glass investigations below explain how the display pivot
was reached. They are historical evidence, not descriptions of the active
display architecture, and are superseded by the baseline above wherever they
conflict.

1. **D3D11 windowed apps render transparent — investigate & fix.** Windowed
   D3D11 swapchains (FaceWorks, Fire Strike windowed) show a transparent/black
   client area even when placed on-screen at the right size, while the desktop
   and window frames composite fine. Established this session: the app DOES
   render correct content (`HELIOS_PRESENT_READBACK` source non-black at
   1264×681); it is NOT alpha (`HELIOS_PRESENT_FORCE_OPAQUE` no-op, owner-
   confirmed); it is NOT a two-memory split (the KMD adopt path backs the alloc
   with the DXVK venus image — an earlier "KMD zeroes the resid" reading was a
   UMD struct-layout misread, see below); and it is NOT the IDD (both the D3D11
   fallback and the dead D3D12 path capture the same single composed IddCx
   surface — D3D12 is dead only because our UMD has no D3D12). The live thread:
   **DXGI `EnumOutputs`/`GetDisplayModeList` racily return `0x887a0022`** on the
   Helios adapters, DWM never imports the app's flip backbuffer (its max
   imported resid trails the app's). **ROOT-CAUSED 2026-07-08 (34th) — see
   `WINDOWED_BLT_DESIGN.md` (full design + implementation plan) and memory
   `windowed-blt-occluded-root-34th-session`.** It is specifically the legacy
   **`DXGI_SWAP_EFFECT_DISCARD` (BLT) swap model** returning `DXGI_STATUS_OCCLUDED`;
   **flip composites fine** (proven with `tools/d3d11_triangle.cpp`). Falsified:
   alpha, IDD, two-memory-split, phantom-LUID/EnumOutputs, adapter selection, AND
   the cross-adapter cap (present is same-adapter, not cross). Real cause: a legacy
   DISCARD windowed present needs a **real active VidPn output** in the path; ours
   has none — the only monitor is the indirect IddCx one, enumerated on Helios's
   **runtime-synthesized *facade* output** (`NumOfSources=0`), and there is no real
   display adapter to cross-adapter to. FIX = give Helios a **real (virtual) VidPn
   source** (RDP/display-miniport model) so DISCARD resolves a real output; do the
   **Stage-0 WARP A/B first** (WINDOWED_BLT_DESIGN.md §6). The "two Helios adapters"
   are the Helios render adapter + the Looking Glass IddCx adapter (which inherits
   the render adapter's name) — NOT stale residue.
   **STAGE 0 DONE + STAGE 1 IMPLEMENTED (2026-07-08, 35th):** Stage 0 validated
   Option A on-screen (disable Helios → WARP presentable → BLT composites). §6.3
   resolved from MS docs — a VidPn source+target+monitor must be same-adapter, so
   Helios gets its OWN 2nd (virtual, no-scanout) monitor (owner-approved; IDD
   renders unchanged, monitor unobserved). **Built KMD v22.22.63.0** with the full
   display half behind a `DisplayHalf` REG_DWORD knob (default 0 = today's
   render-only surface): `start_device` sources/children=1 + child DDIs +
   GetChildContainerId; new `ddi/vidpn.rs` (viogpudo-style
   EnumVidPnCofuncModality/RecommendMonitorModes, single 1920x1080@60); VidPn DDIs
   in `display.rs` (IsSupportedVidPn=TRUE, RecommendFunctionalVidPn=NO_RECOMMENDED,
   Commit/SetAddr/SetVisibility=SUCCESS no-op scanout). Compiles + signed; NOT yet
   installed. **NEXT: owner install (reboot) → `DisplayHalf`=1 + `pnputil
   /restart-device` → BLT triangle un-occlusion test** (WINDOWED_BLT_DESIGN.md §9,
   memory `windowed-blt-display-half-implemented-35th`). Honest caveat: docs don't
   tie OCCLUDED to a VidPn source — the knob A/B is the arbiter.

2. **Slow first-paint on some windows — our UMD makes DWM wait.** Settings app,
   parts of Explorer on fresh open, and (easiest repro) the **UAC dimmed
   window** take several seconds to render. Suspected a UMD-side present/consumer
   wait or a per-window gate stalling DWM's first composition of these surfaces.
   Likely related to #1's consumer/import path. NEXT: measure — instrument the
   present-wait / gate-flush / consumer-wait counters against a UAC-window repro,
   find which wait blocks and why it only bites first-paint.

3. **Codebase cleanup (HIGH).** Many paths accreted across bring-up sessions add
   overhead or cause minor misbehaviours: retired diagnostic scaffolding, dead
   knobs, superseded present/staging paths, force-* diagnostics, staged-probe
   machinery, and now-falsified experiments (e.g. the `DECLARE_CROSS_ADAPTER_RESOURCE`
   line, broad-adopted-BAR remnants). Audit the UMD present path, dxvk-helios
   staging/refresh layers, and KMD segment/adopt code; delete or gate what is not
   load-bearing, with before/after behaviour verified. Do this before large new
   feature work so #1/#2/#4 land on a clean base.

4. **Performance — Fire Strike fullscreen (~100 fps @1080p, GT1).** Owner
   believes near-2× is reachable. Render path is healthy (fullscreen renders
   correctly). Measure first (venus submit/fence latency, copy/acquire gates,
   present-to-scanout) then remove known costs. See WS2 for the levers already
   mapped (feedback-shadow retire, dcomp vehicle, copy-latency).

## Workstream 1 — Stability

**IDD frame freeze: DIAGNOSED 2026-07-05 (17th session), live on the frozen boot** — full chain
in memory `idd-freeze-root-cause-chain`. Summary: (1) routine multi-second completion stalls
(per-present full-GPU drain in `rotate_resource_backings` + event-cadence desktop) →
(2) the 4×8 s sem-deadline latch declares CONTEXT LOST on a healthy-but-slow stack →
(3) dxvk teardown on DEVICE_LOST resets command pools with host work pending (= the
`vkResetCommandPool` VUs; symptom, not cause) → (4) post-loss, `submitCmdLists` drops cmdlists
WITHOUT `notifyObjects()` → in-use refs leak → next `Map` → `waitForResource` (no timeout, no
lost-check) wedges dwm permanently; win32k session-1 GDI hangs behind it. Falsified: the
early-fence/helios_sync theory for the steady-state stall — 0 of 251,810 submissions carry
ring≠0; the vn win32-sync signal path never fires; cross-process sync is dxvk-helios-internal.
**Status 2026-07-06 (18th session): the whole chain is now closed** — (1) the "stall" was the
sem-deadline misreading idle wait-before-signal waits (fixed, defect 1 below) plus the rotate
drain (fixed, WS2); (2) the latch no longer fires on idle desktops; (3)+(4) fixed 17th session.
Remaining: cold-boot + multi-hour soak, and the forced-loss test for the loss path (defect 2).

Open defects, roughly ordered:

0z. **NEW, 2026-07-27 (R614 gate): the Mesa venus ICD does not survive adapter teardown —
   `pnputil /restart-device` ACCESS-VIOLATES every process holding a venus device.** Each
   restart-device cycle logs Application-log id 1000 faults with
   `Faulting module name: vulkan_virtio-<hash>.dll, Exception code: 0xc0000005` for dwm.exe plus
   3-4 shell processes (Explorer, SearchHost, StartMenuExperienceHost, ApplicationFrameHost,
   ShellExperienceHost). The desktop self-recovers — dwm restarts and composites, which is why
   this was never noticed — but "a new dwm pid is expected, not a crash" was too generous: it IS
   a crash, just a survivable one. The same cluster appears at every clean shutdown/reboot.
   **PRE-EXISTING, NOT caused by R614, and the evidence is unambiguous:** 889 such faults in the
   Application log spanning 2026-07-07 → 2026-07-27, and **T4a's own nine-consecutive-restart-device
   soak on 22.22.184.0 produced them on every cycle** (08:56:45, 08:56:58, 08:57:12, 08:57:25,
   08:57:39 … a ~13 s cadence matching the nine cycles).
   **Why every gate so far called that soak clean:** the gates check the SYSTEM log for
   4101/dxgkrnl/LiveKernelEvent, and those stay legitimately empty — a user-mode ICD access
   violation is not a TDR. Nothing checked the APPLICATION log. `tmp/r614-tdr-check.ps1` and
   `tmp/r614-icd-fault-history.ps1` now do; fold them into the standing gate.
   Next step is a stack: the deployed UMD/ICD PDBs are present, so
   `tools/take-minidump.ps1 -ProcessId <dwm>` under a restart-device, then
   `minidump-stackwalk` on Linux, should name the ICD site directly. Most likely shape is the ICD
   touching a venus object (or its ring/reply BAR mapping) after StopDevice destroyed the host
   context — i.e. the ICD has no adapter-loss path, which is the same class as the 17th-session
   DEVICE_LOST chain but on teardown rather than on a slow present.

0a. **Ghosting — ROOT-CAUSED AND FIXED IN LAYERS (21st session, 2026-07-06).** Owner
   insight proved out: the dirty-rect *attribution* was fine; STUTTER caused the
   ghosting. Evidence chain (all same-day): dwm's composed primary is paintcap-CLEAN
   under a bouncing-window probe while the LG client trails catastrophically → the
   corruption is in the IDD→KVMFR→client delivery; 1843 consecutive IddCx acquires
   show frame-delta exactly 1 (no skipped presents) and zero move-regions; WUDFHost
   consumer waits showed timeouts=0 while trails persisted → the wait "succeeded"
   against the WRONG instant. Root cause: the WS1 #4 consumer wait ran at cmdlist
   START (refreshHeliosStagedImages), re-reading the publish slot when the list
   BEGAN — under load that predates the acquire the list's copy serves by a full
   consumer cycle, so the wait targeted an already-retired value and the copy read
   ring-stale content inside freshly-reported damage rects. Fixes landed:
   - dxvk-helios `6eab004c`: bounded present-wait ON THE CS THREAD at copy-execution
     time (copyImage/copyImageToBuffer, imported sources). The acquired buffer can't
     be re-presented while held, so the slot value at that moment IS the acquired
     present's value. Silent no-slot returns (unordered reads) + fast-path hits now
     counted in the `present-wait:` line (`fast=`, `noslot=`).
   - dxvk-helios `35fe0912`: WUDFHost.exe app profile heliosPresentWaitUs=500000 —
     the copy-time wait exposed real producer lag (dwm fence frozen 250ms+ behind
     its publishes under churn; 32ms bound timed out exactly when ordering mattered).
     dwm keeps the tight 32ms default.
   - LGIdd `13630d7f`: D3D11 readback path (the ACTIVE path — D3D12 device creation
     fails 0x887A0004 by design, the D3D12 partial-copy path is DEAD code) finishes
     the acquired frame AFTER the staging Map (was: at CopyResource submit — dwm
     could re-render the buffer before the copy executed), reuses the staging
     texture (was: CreateTexture2D per frame = venus alloc/free per frame), adds
     per-stage QPC telemetry (`D3D11 path stats:` 1 line/300 frames), and a
     `HeliosForceFullDamage` registry knob (diagnostic; owner-verified to hide
     ghosting).
   - LGIdd `37356f83`: pending-damage debt — frames dropped after their dirty rects
     were consumed (LGMP queue full fired exactly at the login→desktop transition:
     giant permanent cold-boot ghosts, owner screenshot) bank their damage for the
     next delivered frame; new-subscriber re-post now carries full damage
     (damageRectsCount=0) instead of the last frame's partial rects.
   - LGIdd `5d36c512`: IddCx MOVE REGIONS delivered as damage (DestRect per move) —
     previously never queried, silently dropped (window drags are moves+edge-dirt;
     zero moves observed from programmatic MoveWindow, so this wasn't the trail
     cause, but it was a real hole).
   - LG client `b27eb1d0`: malformed frames (zero geometry) and transient
     onFrameFormat failures skip-and-retry instead of exiting — a client exit tears
     down the whole VM via the launcher (observed live: render-server killed dwm's
     venus context → torn frame → client exit → VM shutdown).
   Remaining for 60fps IDD (owner target): the serialized IDD cycle measured
   map(wait-inclusive)+memcpy ≈ 60-95ms under churn. `AllocCached` KMD fix (22.22.53)
   targets the 36ms WC-read memcpy; producer completion lag (dwm CS submission lag +
   a RECURRING ~1.49s stall, constant across configs — hunt with QMP fence tracing)
   is the other half. See WS2. **22nd session: the ~1.49s stall is ROOT-CAUSED AND
   FIXED (our own staged-probe diagnostics — see WS2); present-wait timeouts now 0.**

0b. **NEW DEFECT — venus pipeline-layout lookup failure killed dwm's context
   (2026-07-06, host-side evidence).** virgl render server:
   `vkr: failed to look up object 626238 of type 19 (pipeline layout)` →
   `vkCreateGraphicsPipelines resulted in CS error` → fatal decoder state → context
   1613 (dwm.exe) destroyed → all subsequent blob creates refused → LG client choked
   on the torn frame and exited → launcher shut the VM down. First host-side proof
   of a guest venus protocol violation.
   **22nd session (2026-07-06): the fork-early-out suspicion is FALSIFIED as the
   cause.** Static audit: vn_pipeline.c/vn_common.c are byte-identical to upstream
   (the relaxed tail load in wait_all is upstream's own); the fork's divergence is
   confined to vn_ring.c. Evidence: the never-read ring-diag file at
   `C:\Windows\Temp\helios_icd_diag.log` (13.5 MB across 6 boots) shows the
   shared-valid early-out NEVER fired and the wait_seqno fatal-abandon fired
   exactly twice — both AFTER the host had already latched ring-FATAL
   (consequences of the CS error, not causes; on the death boot the first
   guest-side anomaly IS the post-FATAL abandon, head frozen at 141452 with
   ~1.2 KB undecoded). The sync-vs-async structure is also sound: TLS-ring
   pipeline creates are `vn_call_` (synchronous), so destroy-after-create races
   are excluded. The initiating violation left NO guest-side trace. Landed
   (mesa `a0412461012`, ICD `vulkan_virtio-7c9ddf378055` deployed):
   `vn_ring_wait_all` barrier skips/abandons now log LOUD
   (`BARRIER SKIPPED/ABANDONED in wait_all`) to the ProgramData diag log —
   either line on a healthy process is the smoking gun; ring diag rerouted
   from `C:\Windows\Temp` (unwritable for WUDFHost — its ring diags silently
   vanished) to `C:\ProgramData\Helios\helios_icd_diag.log`; post-fatal
   per-submit spam rate-limited (power-of-two); NEW env knob
   `VN_HELIOS_PIPELINE_TRACE` traces (ring, primary_tail seqno, object id)
   for layout create/destroy + the barrier + pipeline creates. NEXT on
   recurrence: read the BARRIER/PL-TRACE lines before theorizing; host-side
   `HELIOS_VKR_DEBUG=validate` relaunch (ask owner) — NOT VIRGL_LOG_LEVEL
   (HOST.md §5.1: venus runs in the render-server child; only WARN+ reaches
   the qemu stderr log).

1. **Stall — ROOT-CAUSED AND FIXED (18th session, 2026-07-06).** The strike attribution
   (mesa `f7a816f182f`: sem id, wait reason, signal queue/family/ring, signal value+age in the
   strike line) cracked it in one boot: **every observed strike had `sig_age_ms ≤ 14`** — the
   waiter had legally parked a wait-before-signal on the NEXT frame's timeline value while the
   desktop idled; the moment the next frame's burst submitted the signal, the old deadline
   (which measured time-since-counter-movement, gated only on a-signal-is-pending-NOW) fired
   against a milliseconds-old signal. Explorer striking at exactly hh:mm:00 = the taskbar clock
   tick submitting that signal. The strikes were false positives BY CONSTRUCTION on an idle
   desktop, and 4 of them during login churn are what tripped the DEVICE_LOST latch (act one
   of the 2026-07-05 freeze cascade). FIX: the deadline now measures how long a submitted
   signal has been pending with zero movement (`pending_signal_since_ns`); a genuine zombie
   (Xid-109: pending forever) still latches at strikes×deadline. RESULT: strikes went from
   ~1/min/process to ZERO across dwm+explorer+WUDFHost (fixed ICD, ~10 min incl. idle).
   The second stall contributor — the rotate drain — is fixed under WS2 (presents now track
   damage rate: ~10/s under the 10 Hz flasher vs ~6/s before). 19th-session soak data
   point (2026-07-06, warm boot): ZERO non-forced strikes and zero DEVICE_LOST over the
   whole session (~1 h incl. heavy GPU probe workloads); present-gate timeouts flat at the
   startup value; rotate-perf 1–4 µs; HELIOS_QUEUE_PERF live in dwm, submit phases all
   µs-class (submit_avg ~4 µs). Remaining: multi-hour/day soak.
2. **dxvk-helios device-loss hygiene — FIXED and FORCED-LOSS-VERIFIED (19th session,
   2026-07-06):** (a) dropped post-loss submissions now `notifyObjects()` + recycle (was:
   permanent in-use ref leak → dwm compositor wedge); (b) `waitForResource` bails on
   DEVICE_LOST (loud warn); (c) on lost, bounded 2 s grace wait then deliberate cmdlist
   leak instead of resetting a pool with host-pending buffers (was: the vkResetCommandPool
   VUs). End-to-end forced-loss test PASSED (`VN_HELIOS_SEM_DEADLINE_MS=1` +
   `_STRIKES=1` on d3d11_upload_integrity_probe): latch fired on the first genuine 1 ms
   pending window with full attribution, probe ran to completion with loud FAILs (reads
   return zeros post-loss) and exited — no wedge; all three hygiene paths logged in the
   dxvk log (`leaking command list with unretired host work`, `waitForResource aborted`,
   repeated loud submit failures); dwm untouched. Note: once the ICD latches, host
   retirement becomes unobservable, so the deliberate leak fires even on a healthy host —
   by design.
3. **dwm shared-resource creation failure — FALSIFIED as written (18th session, 2026-07-05)**:
   every `DXVK memory not importable` line (current boot AND the freeze-evidence tail) belongs
   to `misc=0x0` PRIVATE textures, where suballocation is correct. Shared/present creations
   already get dedicated importable memory (`forceDedicated`, dxvk_image.cpp:634): dwm's
   backbuffers allocate `kind=DEVICE_MEMORY` blobs (blob=venus mem id, KMD creates the wire
   resource) and the IDD imports them (resids 32/33/36, live content probe-verified). The
   `res_id=0` in the allocate log is normal for KMD-created blobs, not a failure. Log line now
   scoped: loud `SHARED RESOURCE WITHOUT IMPORTABLE BACKING` only when the resource actually
   needs one (shared/keyedmutex/present/primary); private-texture noise trace-gated. The IDD's
   per-acquire re-resolve is the alias-staging refresh design, not a missing-export symptom.
   Residual noise, harmless: dwm warns `Failed to write shared resource info` 9× (dxvk shared
   metadata runs before the UMD stamps KMT handles; WINE-escape fallback fails by design).
4. **KMD wire-fence semantics / consumer-side present ordering — MECHANISM PROVEN
   END-TO-END (19th session, 2026-07-06).** `tools/vk_ring_fence_probe.cpp` (schtasks
   `helios_ringprobe`, MEDIUM integrity — see tooling gotcha) demonstrates the full chain
   on the live stack: vkQueueSubmit2 signaling an exported win32 timeline semaphore →
   vn ring_idx≥1 fence-only SUBMIT_VENUS (`vkWaitRingSeqnoMESA` cs, orders the fence
   behind the ring-decoded queue submission) → KMD INFO_RING_IDX wire fence (the transport
   already honored ring_idx; in-flight table is per-fence token-matched, out-of-order-safe)
   → QEMU 11.0.1 → proxy → render-server vkr queue sync thread → host GPU completion
   (qemu fence trace: retire at 263 ms on a 269 ms workload; ring-0 retires in µs at
   decode) → used-ring completion → **new ICD retire thread** (mesa `4a6aa14f17b`: on
   every SHARED-sync signal, a per-renderer thread WAIT_FENCEs the wire fence and signals
   the shared WDDM monitored fence in retire order; helios_sync now refcounted; also fixed
   the `helios_sync_append_locked(fence_id=0)` blind-signal that cleared older pendings)
   → consumer `vkWaitSemaphores` on the re-imported semaphore returns at 97–98 % of
   T_gpu, never early. KMD `22.22.52` (BUILT, NOT yet installed — needs owner reboot):
   WDDM pending-FIFO watermark now counts ring-0 fences only (ring≥1 stay in flight for
   the whole GPU-work duration; counting them would couple GDI/paging DMA pacing to
   multi-ms GPU work) + RING_SUBMIT/RING_COMPLETE counters (TDR report v4).
   **PRODUCTION INTEGRATION DEPLOYED (20th session, 2026-07-06) — A/B running with
   `PresentGateUs=0`.** The design changed twice against reality:
   - **KMT is impossible**: dxgkrnl rejects a MONITORED fence with `Shared=1` and no
     `NtSecuritySharing` (0xc000000d, proven live), i.e. no global-DWORD flavor exists.
     The ICD's legacy-D3DDDI_FENCE fallback silently produced syncs whose FromCpu waits
     hang forever (wedged the probe) — fallback REMOVED, KMT semaphore caps no longer
     advertised (mesa `d5d698aaec5`). Rendezvous = **NAMED NT sharing**: standard
     `VkExportSemaphoreWin32HandleInfoKHR::name` / import-by-name
     (`D3DKMTShareObjects` w/ named OBJECT_ATTRIBUTES → `D3DKMTOpenSyncObjectNtHandleFromName`).
     dwm CAN create `Global\` names (SeCreateGlobalPrivilege verified on its token).
     `vk_ring_fence_probe named` rc=0 (98 % of T_gpu; NT regression 97 %).
   - **LGIdd needed NO changes**: its per-acquire copy runs on OUR UMD/dxvk inside
     WUDFHost (that instance imports dwm's backbuffers as alias images — resids
     probe-verified) — the consumer wait lives in dxvk-helios's
     `refreshHeliosStagedImages`, `dxvk.heliosPresentWaitUs` (default 0, WUDFHost.exe
     profile = 100000; imported-mode images only, so dwm never grows a CS-thread wait).
   Shipped shape: producer (`present_sync_publish`, umd `9d22620`+`a7f2dfa`, dxvk
   `b15bc42f`+`8c612b3d`) creates one named fence per D3D11 device
   (`Global\HeliosPresentFence_<pid>_<fenceId>` — dwm owns SEVERAL devices, per-pid
   names collided live; Everyone-DACL), records signalFence(++counter) on the frame's
   OPEN cmdlist (no present-thread wait), publishes (resid → pid, fenceId, value) in a
   seqlock-slotted mapped FILE `C:\ProgramData\Helios\helios_present_sync.bin` (both
   principals have ProgramData rights; do NOT delete the live file — mapped views
   split-brain until the mappers restart). Consumer imports by name (cached per
   pid+fenceId), `getValue` fast-path, bounded wait, timeout → copy anyway + loud
   `present-wait:` telemetry. Kill switches: `HKLM\SOFTWARE\Helios!PresentSyncPublish=0`
   (producer), `dxvk.heliosPresentWaitUs=0` (consumer). Deployed: UMD
   `helios_umd_8301bc9779a48b99.dll`, ICD `vulkan_virtio-cf1280da750c.dll`, verified in
   dwm+WUDFHost; cross-session import proven live ("imported fence of producer pid").
   First numbers (gate=0, 10 Hz flasher stress): ~900 consumer waits, avg 5–6 ms,
   ~4 % bounded timeouts in bursts when dwm's CS runs >100 ms behind (structural:
   the consumer chases the latest published value; bounded + loud by design), ZERO
   strikes, desktop paintcap-clean, submit_avg ~4 µs unchanged.
   **Owner cold-boot report (same session): ghosting + frame drops PERSISTED — the
   WUDFHost-only scoping was WRONG.** The old gate had been ordering EVERY producer's
   present (apps included), so registry gate=0 also unordered the app-backbuffer →
   dwm-composition edge; the artifacts were in dwm's OWN composed primary (stale
   sliver + content overhang in the owner's screenshot). Fix (dxvk `90b76c5c`, UMD
   `helios_umd_f2c6f833d7d293eb.dll`): `heliosPresentWaitUs` defaults to **32000 for
   every consumer** — the wait fires only for imported surfaces with a published
   slot (dwm/IDD cross-process edges), the wait DAG is acyclic (IDD → dwm → apps),
   every edge bounded; uniform 32 ms also caps the IDD stall bursts that read as
   frame drops (was 100 ms). Verified live: dwm imports app producer fences
   ("imported fence 1 of producer pid <app>"), zero dwm-side timeouts, churn
   paintcaps coherent. Timeout warns now log the fence's current value — churn
   bursts are retire-lag (publish outruns GPU completion by a few frames).
   **Remaining:** owner eyeball via Looking Glass + multi-hour soak with gate=0;
   then retire `PresentGateUs` (flip compiled default to 0) and drop the gate from
   the hot path. Differential levers if ghosting recurs: `DXVK_CONFIG
   "dxvk.heliosPresentWaitUs = N"` per process, or registry `PresentGateUs=32000`
   restore. Known residue: old-ICD probe runs leaked 4 idle render-server workers
   host-side (virgl-63/65/95/97); clears on VM restart, did not recur with the new ICD.
5. **dxgkrnl "Driver returned an invalid NTSTATUS 0xC00000BB"** (ETW
   AzureTriage) — some query answered STATUS_NOT_SUPPORTED where that return
   is illegal. Tolerated today; find and fix the query.
6. **WUDFRd cold-boot race** ("SCM not ready", boot+23s) — LGIdd loads late;
   pairing is resilient now but the race window is still there.
7. **In-place KMD update flakiness** — CM_PROB_FAILED_POST_START limbo until
   reboot is expected, but keep the version-coherence gotcha (one site since
   2026-07-26: `kmd_render/driver-version.env`) and
   backup ladder in mind. 2026-07-06 state (19th session END, reboot-verified): ACTIVE
   driver = **oem59.inf = 22.22.52 (pkg 155b7345f9360525)** — per-ring watermark +
   counters live. `UserModeDriverName` → ProgramData `helios_umd_b3615be0ce9de13e.dll`
   (RELEASE profile), dwm ICD = **`vulkan_virtio-5535366186bd.dll`** (retire thread) —
   both verified by the fresh dwm's loaded-module list + paintcap. **NEW GOTCHA: a KMD
   `devcon update` install creates a new DriverStore dir and RESETS `UserModeDriverName`
   to the DriverStore copy, which ships the package's DEBUG-profile UMD — after EVERY
   KMD install, rerun `win_install_umd` (release dll + `-KillUmdUsers -RestartDevice
   -NoProbe`) and re-verify dwm's loaded module. The DriverStore UMD copy staying locked
   during that redeploy (script exit 1) is benign — ProgramData + registry are what
   load.** Backup: `C:\ProgramData\HeliosDeployBackups\20260706-021734`.
8. **GDI byte-path / executor retirement (REFRAMED 20th session)**: post-cold-boot,
   GDI content renders while RenderGdi (GdiE), MapCpuHostAperture (ChMn) and
   paging (Pg*) counters all stay idle — the canonical CPU path likely already
   carries the bytes. RESEARCH CONFIRMED (2026-07-06): GDI HW acceleration is
   OPTIONAL (`SupportKernelModeCommandBuffer` is a "should… only if" per
   gdi-hardware-acceleration.md); **viogpu3d** (vendored kvm-guest-drivers, the
   closest existing WDDM driver to Helios, near-identical cap set) never sets the
   bit and implements NO RenderKm/RenderGdi; WARP works render-only in the IDD
   slot. The in-tree "LOAD-MANDATORY" bisect (query_adapter_info.rs:166,
   2026-07-02 v22.22.34) predates Option A and is confounded by the then-broken
   CPU-visible story — retest under BarSegMode 10 via a new `GdiAccelMode`
   service-key knob + pnputil restart-device (AzureTriage names any FAILED_ADD).
   If the desktop renders accel-off: verify byte flow (GdXn stays 0, watch for a
   two-memory-split regression = black/stale GDI windows), verify the BAR CPU
   mapping is WB-cacheable (CPU GDI is read-modify-write), re-run the Doom
   stutter differential (its WSI BitBlt storm currently traverses the executor),
   then retire the gdi_blit executor (a guest-CPU blitter behind a DMA round
   trip — strictly worse than win32k's rasterizer, source of the 48%-drop bug
   class).
   **21st session (2026-07-06): both knobs SHIPPED in 22.22.53** (`85ad16a`):
   `GdiAccelMode` (default 1) gates SupportKernelModeCommandBuffer per the plan
   above; `AllocCached` (default 1) additionally answers the WB-cacheable
   question for ALL CpuVisible allocations — user views were mapped WC (only
   CpuVisible was set at create_allocation; the WDDM2 `Cached` flag was never
   set), measured at ~200 MB/s reads = 36 ms per 7.8 MiB IDD readback frame.
   **22nd session: GdiAccelMode=0 A/B PASSED — the 2026-07-02 "LOAD-MANDATORY"
   bisect is OVERTURNED under BarSegMode 10.** Evidence (all same-boot, via
   `pnputil /restart-device`): adapter re-adds `CM_PROB_NONE`; desktop composes;
   classic-GDI text renders (fresh cmd-console canary echoes + regedit tree text
   + desktop-listview labels, paintcap-verified); every Gd* counter FROZEN this
   boot (GdiE 339, GdXn 24 — zero executor traffic; the KMD's `GdiM` mirror
   flipped 1→0 proving the mode took); no two-memory-split regression under
   mover churn across three device restarts. **The knob is LIVE at 0 for soak**
   (service-key value set; revert = `reg delete ...\helios_kmd_render /v
   GdiAccelMode /f` + restart-device). NEXT: owner-attended Doom stutter
   differential (its WSI BitBlt storm no longer traverses the executor), then
   retire the gdi_blit executor + flip the compiled default.
   **CLOSED 2026-07-26 (T1b, 22.22.180.0)** — owner directive: the driver does
   not and should not advertise GDI acceleration, so both the advertisement and
   the executor are gone (R903/x-dup-dead-20, pulled forward from T6).
   Reachability was re-proven the same boot before deleting anything: every
   `Gd*` service value deleted, then explorer restart + maximized notepad + GDI
   canary + repaint + EnumWindows + two paintcaps, and **not one `Gd*` value
   reappeared** (the executor flushes its block on its first batch, so absence
   is proof, not throttling). `SupportKernelModeCommandBuffer` is now hard-coded
   0; `DxgkDdiRenderGdi`/`RenderKm` stay registered as record-and-advance
   (a null slot bugchecks — `DdiRenderGdi+0x140`). This also deletes T1b's
   R301/R304/R305/R306, which only hardened the deleted executor.

## Workstream 2 — Performance

- **OPEN (2026-07-26, found while taking the T0 baseline): DComp cadence ~50 fps, not the
  documented ~63; present-gate avg ~2.0 ms, not the documented ~0.48 ms.** Measured on an
  idle box (CPU ~1 %) with `helios_dcomp_probe` (25 s runs): 1236 / 1152 / 1253 frames
  **before** the T0 deploy and 1227 / 1307 **after**, i.e. 46–52 fps throughout — so this
  is NOT a T0 effect and not a debug-vs-release-UMD effect. An earlier stored run of the
  same probe on this box recorded 1576 frames (63.0 fps), which is where the documented
  figure comes from. dwm `present-gate:` reads avg 2018 µs / max 14595 µs / 30 timeouts in
  3072 presents on the release UMD. Not yet triaged: unknown whether the regression is in
  the guest (present path), the QEMU frontend, or host-side scanout delivery — the PSC
  charter says to measure present-to-scanout and VNC delivery separately before assigning
  blame. Repeatable baseline procedure: two `helios_dcomp_probe` runs, then read the last
  `present-gate:` line from the live dwm UMD log.
- **IDD delivered-frame cycle — MEASURED (21st session, `D3D11 path stats:` line,
  1/300 frames in the LGIdd log):** the swapchain thread is fully serialized, so
  stage sums ARE the delivered fps. Under a 50Hz bouncing-window probe:
  rects ~14µs | staging ~0 (reuse landed; was a venus CreateTexture2D per frame) |
  copy-submit ~25µs | map 13-59ms avg with a RECURRING ~1.5s max (suspiciously
  constant across configs/builds — a fixed timeout somewhere; hunt with QMP fence
  tracing `virtio_gpu_fence_ctrl/resp` during a mover run) | memcpy 34-38ms
  (7.8 MiB at ~200 MB/s = WC reads) | lgmp ~0. Owner target: 60fps IDD.
  **RESOLVED IN TWO STEPS (same session):** (1) `AllocCached` (22.22.53) did NOT
  move the stage — the ICD maps venus blobs through its own escape path using the
  host's per-blob map_info (the WDDM2 Cached flag only affects dxgkrnl-owned
  mappings); a one-shot BW probe (now permanent in LGIdd) split the sides:
  src strided-read 622 MB/s, memcpy 157 MB/s, IVSHMEM memset 27 GB/s — the READS
  of the WC venus mapping were the whole cost. (2) LGIdd `1d03b685`: MOVNTDQA
  streaming loads (CopyFromWC) → probe streamcpy 5.5 GB/s, frame memcpy stage
  36.5ms → **1.4ms**, delivered rate 14-18fps → **32fps = the probe's damage
  rate** (map now ~8ms avg; pipeline headroom ~100fps — a 60Hz producer should
  see 60).
- **The RECURRING ~1.5s stall — ROOT-CAUSED AND FIXED (22nd session,
  2026-07-06, dxvk-helios `bdbbc2ea`): it was OUR OWN staged-content-probe
  diagnostics.** QMP fence tracing (`virtio_gpu_fence_ctrl/resp` during a mover
  run) split it in one pass: host ctrl→resp ≤ 11.5 ms across 7891 fences (p50
  0.17 ms — host exonerated again), but ALL guest submissions went SILENT in
  ~1.49 s beats, 3 per cluster, every 600 IddCx frames, landing at tick
  600k+30 = the probe HARVEST tick (`HeliosProbeRecurTick=600`,
  `HeliosProbeHarvestTick=30` in refreshHeliosStagedImages). The harvest
  scanned the ~7.8 MiB probe readback BYTE-WISE through the WC venus mapping
  ON THE CS THREAD (~0.75 s per probe at WC single-byte rates × 2 probes per
  staged image × 3 imported dwm backbuffers = 3 beats/cluster), while WUDFHost
  held the acquired IddCx frame — starving dwm and every producer behind it.
  Corroboration: probe issue/result log lines at ticks 2400/3000/3600 match
  the stall frames exactly; the 48-probe cap matched the observed stall
  cutoff (last cluster at tick 3600, none after); this also explains the
  "constant across configs" property (the probes shipped in every build), the
  LGIdd map-max 1.47-1.54 s, the "dwm fence frozen ≥500 ms" producer-lag
  signature, AND all 10 consumer present-wait timeouts (fence exactly one
  behind = dwm's next signal parked behind the block). FIX:
  `dxvk.heliosStagedProbes` config knob, default OFF (re-enable per process
  via DXVK_CONFIG for black-surface triage); harvest now bulk-copies WC →
  cacheable before scanning (~50 ms, not ~1.5 s, if ever re-enabled).
  VERIFIED live (mover churn re-trace, UMD `helios_umd_81a033e237bef769.dll`):
  max silence 48.5 ms (was 1493 ms), submission rate flat through every
  600-frame boundary, `present-wait: timeouts=0`, desktop paintcap-clean.
- **Producer (dwm) completion lag:** publishes outrun the fence by 1-4+ presents
  under churn; vkQueueSubmit2 phases are µs-class (queue_perf), so any residual
  lag is dxvk-CS-thread backlog + venus decode/retire path, NOT the submit
  call. The dominant "frozen fence" signature was the staged-probe stall
  (above, fixed). RE-MEASURE under 60Hz content before more work here;
  if residue: instrument dwm CS latency (record→execute) and the ICD retire
  thread's WAIT_FENCE throughput; consider moving dwm's own consumer waits from
  list-start to copy/sample-time like the IDD fix.

- **Per-present full-GPU drain — FIXED (18th session, `3579ef7` + `ef6689b`):**
  `rotate_resource_backings` drained the whole device (event query + `Sleep(1)` spin —
  the timer quantization WAS the 15–25 ms) on dwm's present thread every present. Now a
  CS-side identity rotation mirroring upstream `D3D11SwapChain::RotateBackBuffers` via
  `InjectCsOrderedAfterPending` (dispatch open chunk + ordered inject, NO CS-thread wait —
  the first iteration's `SynchronizeCsThread` blocked up to **1.9 s/present** behind login-
  churn CS backlogs = the post-cold-boot "occasional framerate dips"). `rotate-perf` after:
  **1–4 µs/rotation**; presents track damage rate (~10/s under the 10 Hz flasher, was ~6/s).
  WARNING for future work: do NOT per-image `waitForResource` on the present thread — a
  bound backbuffer RTV is re-recorded into every new open cmdlist, so `isInUse` never
  clears and dwm wedges (proven with a live minidump).
- **Ghosting after the drain removal — GATED (18th session cont., `4f0a96c`):** the drain
  had been accidentally serializing dwm's GPU work against the IddCx consumer's per-acquire
  copy; with it gone, the copy occasionally read a just-presented buffer whose GPU writes
  were in flight (dwm's venus rendering produces NO dxgkrnl-visible DMA fences — nothing
  else orders the cross-process read). Fix: bounded frame-completion gate in the DXGI
  present DDI (`HeliosWaitFrameComplete`: flush, then poll the submission fence — signals
  at GPU completion) before `pfnPresentCb` makes the flip visible.
  `HKLM\SOFTWARE\Helios!PresentGateUs` (default 32000; 0 = off, the A/B lever). Measured:
  avg 3.6–5.3 ms, timeouts only during startup churn, present rate unaffected
  (`present-gate:` telemetry, 1 line/128). The ARCHITECTURAL fix stays WS1 #4: per-ring
  wire fences + cross-process win32-sync so the consumer waits GPU-side.
- **Release-profile UMD is the deploy default (18th session `32cf4a4`; made the actual
  default 2026-07-26, T0/R102):** dev profile is opt-level 1; release is opt 2 + thin LTO,
  with `debug="full"` keeping the GUID-matched PDB for minidump symbolization. Both
  `tools\install-helios-kmd.ps1` and `tools\hotplug-helios-umd.ps1` now default `-UmdDll`
  to `umd\target\release\helios_umd.dll`, and the install plan prints `UmdProfile` next to
  `UmdSource` so the deployed profile is a line in the log. Until R102 the defaults were
  `...\target\debug\...`, so a default `win_install_kmd` signed the DEBUG DLL into the
  DriverStore while reporting success — every cadence and wake-latency number taken that
  way measured the wrong binary. Build with `win_cargo crate_dir:"umd" args:["build","--release"]`;
  pass `-UmdDll ...\target\debug\helios_umd.dll` for a deliberate debug deploy. A missing
  release DLL now aborts the install (`UMD DLL not found`) instead of quietly shipping debug.
  dxvk-helios stays meson debugoptimized (-O2 already).
- **Frame-update slowness** (owner-visible): remaining suspects after the drain fix =
  dxvk-helios persistent-refresh (14th session, alias-image staging + per-frame refresh).
  Quantify with `HELIOS_QUEUE_PERF` (machine env set 18th session; reaches dwm after the
  next reboot; one aggregate line per 300 submits to
  `C:\ProgramData\Helios\helios_queue_perf.log`).
- **Diagnostics overhead — GATED 2026-07-05** (default quiet; knobs restore):
  UMD per-op log I/O was open/write/close per line on per-frame paths → now a
  persistent handle + `trace_line!` behind `HKLM\SOFTWARE\Helios!UmdTrace`
  (e88f2c6; umd log 8612→681 lines/30 s under churn). ICD submit/shmem trace
  (33 MB+1.9 MB per session, fopen per submission) behind `HELIOS_SUBMIT_TRACE`
  env (mesa 4bb43194e5d). KMD: S-ring `diag::record` off + gdi_blit's 20-value
  per-batch registry dump deferred to every 64th batch behind service-key
  `DiagLevel` (bf0ab37, **22.22.51 ACTIVE since the 2026-07-05 reboot**, pkg
  c393e58c1b189688 / oem58). Also cleared the stale
  `HKLM\SOFTWARE\Helios!RotateSample=16` debug knob (was CPU-reading back the
  whole swapchain ring every 16 rotations). GOTCHA found while measuring: pids
  get reused and `umd-<pid>.log` appends — delete logs before comparing runs.
  Present-rate deltas across dwm restarts are confounded by IDD-pairing state;
  compare within one dwm session only. 18th-session regression found+fixed
  (`a6780f1`): the persistent Rust log handle made the bridge's `fopen_s`
  (deny-sharing) fail on every call — ALL `[dxvk-bridge]` lines incl.
  rotate-perf were silently dead in post-e88f2c6 builds; bridge now uses
  `_fsopen(_SH_DENYNO)`. If a telemetry stream goes quiet, check for a
  string-in-binary vs lines-in-log mismatch before trusting it.
- **Doom present path — async WSI worker LANDED (22nd session cont.,
  mesa `808c7e4a786`, ICD `vulkan_virtio-1b44e8d36fe4` deployed): 120 → 160 fps.**
  The sw WSI present serialized the frame-fence wait (5-6 ms: GPU frame +
  venus retire) + StretchDIBits (0.65-0.75 ms) on Doom's present thread
  (`helios-doom-wsi-perf.txt` wait_avg/stretch_avg = the 120 fps ceiling).
  Now: per-swapchain worker thread does wait+invalidate+blit; acquire's
  IDLE condvar is the back-pressure (run-ahead = image_count-1); kill
  switch `HELIOS_WSI_ASYNC_PRESENT=0`. vkcube 6,600 fps; worker wait_avg
  4.3 ms now overlaps the app's next frame. **CRASH POST-MORTEM in the same
  arc: an unconditional 5-image sw-swapchain bump crashed Doom at renderer
  init ×2 (idTech sizes per-image arrays to the REQUESTED count — unhandled
  C++ FatalError, Crash.00003/00004). Spec-legal ≠ app-safe; extra depth is
  now opt-in `HELIOS_WSI_EXTRA_IMAGES=N` (default 0), for engines that
  re-query.** OWNER DECISION (2026-07-06): the sw present path is FROZEN at
  160 fps; the next present work is HW-accelerated present. The async sw
  worker remains the fallback path + kill switches. Host-side during Doom:
  p50 fence 0.05 ms; a tight ~10.7 ms class at 53/s = ring≥1 GPU-completion
  fences seeing the pipelined queue backlog (healthy).
  **D3DKMTPresent FEASIBILITY RESEARCH (same session, 26100 SDK d3dkmthk.h +
  live-counter evidence):**
  - `D3DKMT_PRESENT` itself is fully documented (hContext — gate5a already
    owns one — hWindow, hSource allocation, Flags, INLINE PresentHistoryToken
    built by the CALLER). The wall is the TOKEN: every redirected model
    (GDI/GDI_SYSMEM/BLT/FLIP/COMPOSITION) requires `hLogicalSurface`/
    `hPhysicalSurface` (win32k logical-surface handles) or dxgi-private
    rendezvous state (`dxgContext`, `hCompSurf`, `confirmationCookie`,
    `PresentLimitSemaphoreId`) minted only by the D3D runtimes through
    win32k-private (win32u NtGdiDdDDI*) calls. NO documented path lets an
    ICD mint a valid redirected token.
  - **Blt-model is DEAD on 26100 regardless**: the KMD's DxgkDdiPresent
    feasibility trace (display.rs PBcall/PBsrc/PBdst, present since .34-era)
    has NEVER fired across weeks of desktop uptime (dwm, steamwebhelper,
    Settings, taskmgr, D3D11 apps) — modern win32k drives flip/composition
    models exclusively; a real KMD present-blit would serve a path Windows
    no longer invokes. (Also: a KMD GPU blit needs venus Vulkan encoding —
    viogpu3d's equivalent rides VIRGL_CCMD_RESOURCE_COPY_REGION, a virgl
    primitive venus lacks; a kernel venus Vulkan client is weeks of work
    for that dead path.)
  - Composition-swapchain API (documented "present from Vulkan" API) needs
    a real D3D11/D3D12 device at CreatePresentationFactory → recursion veto
    (and no D3D12 UMD exists). DEAD.
  - **Viable roads, pick after owner gameplay numbers:** (1) Helios-private
    independent-flip-at-the-consumer: for an unoccluded fullscreen-sized
    Vulkan window, the ICD publishes (resid, fence, geometry) via the WS1 #4
    present-sync channel and LGIdd's per-acquire copy sources the APP's blob
    instead of dwm's composition — semantically DXGI independent flip
    implemented at the IDD; every piece exists (publisher wire format,
    LGIdd dxvk import-by-resid, bounded consumer waits); the ONE design risk
    is the foreground/occlusion contract (must provably fall back to dwm
    composition the moment the window isn't the whole visible screen).
    (2) Pinned-build flip-model RE: reverse the dxgi↔win32k rendezvous
    (win32u NtGdiDdDDI* on 26100, which Helios pins as the guest build) to
    mint real flip tokens = true zero-copy dwm flip for windowed too; high
    RE effort, build-pinned fragility. (3) sw path stays the composed-window
    contract (already async; blit 0.7 ms; the dwm-side staged upload is the
    remaining per-composite cost). **(4) NEW — the dzn/zink-style dcomp road
    (in-tree PROOF in wsi_common_win32.cpp's dxgi branch, used by dzn):
    flip-model presents WITHOUT minting tokens — DXGI + DirectComposition
    mint them: `CreateSwapChainForComposition(device_or_queue, FLIP_SEQUENTIAL,
    ALLOW_TEARING)` + `DCompositionCreateDevice(NULL)` (dcomp needs NO
    rendering device) + `CreateTargetForHwnd(hwnd)` → `visual->SetContent
    (swapchain)` → `Commit()` — all documented public API. The one real
    device required is the swapchain's presenting device; a WARP D3D11
    device satisfies it with NO helios_umd recursion (d3d10warp.dll,
    self-contained). Frame flow: venus host-visible frame (cached mapping)
    → CPU copy into the WARP backbuffer → Present(0); dwm opens the WARP
    buffer cross-adapter (proven path — this box ran a WARP-composited
    desktop pre-milestone). Throughput ≈ the sw path (the bytes still cross
    guest→host once per composite, plus one extra CPU copy vs StretchDIBits);
    the wins are flip-model pacing/damage semantics, tear control, and no
    GDI redirection surface. Zink itself is only a consumer: it rides the
    underlying ICD's VK_KHR_win32_surface via kopper — on venus that is our
    sw path; dzn is the driver that exercises the dcomp branch.**
    **OUR OWN D3D11 PRESENT MODEL (read 2026-07-06, umd forward.rs
    `dxgi_present`): windowed D3D11 is ALREADY hardware-presented end-to-end.
    The DXGI runtime solves the rendezvous FOR the UMD — `DXGI_DDI_ARG_PRESENT`
    delivers BOTH `hSurfaceToPresent` and `hDstResource` (the win32k
    redirection/destination surface as a first-class UMD resource); the UMD
    does `CopySubresourceRegion(dst←src)` = the present blit runs GPU-side
    through venus (zero CPU bytes), then WS1 #4 present-fence publish + the
    bounded frame gate + `pfnPresentCb(hSrc, hDst)` — the KERNEL mints the
    history token (allowed in kernel mode; that's why DxgkDdiPresent never
    fires — the blit already happened in the UMD). dwm's own presents are
    flip-model onto the IddCx buffers (no dst → copy no-ops). CONSEQUENCE:
    only the VULKAN client class (native VK games + vkd3d-proton D3D12) lacks
    a HW present — a Vulkan ICD has no runtime handing it the destination
    surface; that missing hand-off IS the entire gap roads (1)/(2)/(4) exist
    to fill. Gotcha: `UmdTrace` is cached at device init — toggling it needs
    a process restart to take effect.**
    **ROAD 4 ON OUR OWN ADAPTER — PROVEN LIVE (2026-07-06,
    `tools/dcomp_present_probe.cpp`, schtask `helios_dcomp_probe`):** a D3D11
    device on the HELIOS adapter (our UMD; owner-approved vehicle use — the
    nesting is bounded and dwm already runs multiple DXVK→ICD stacks per
    process) + `CreateSwapChainForComposition(FLIP_SEQUENTIAL)` + dcomp
    target/visual on the HWND: every call S_OK, 1023 flip presents, dwm
    composes the animation (paintcap ×2, gradient advancing). This upgrades
    road 4 from "WARP + CPU copy" to the REAL zero-CPU-byte design:
    the DXGI backbuffers are OUR KMD allocations = venus blobs, so the ICD
    copies its frame into the current backbuffer GPU-side with its OWN venus
    device (import-by-resid, the dwm/WUDFHost machinery), then Present() on
    the vehicle device mints the token via pfnPresentCb (flip model → the
    UMD's dst-copy no-ops).
    **DESIGN REFINED (owner, 2026-07-06): reverse the import direction and
    special-case in the UMD present DDI via D3D10DDI_HRESOURCE.** The ICD
    never learns backbuffer resids (kills the COM→DDI texture-query problem
    entirely): the ICD publishes ITS OWN frame resid + fence value (WS1 #4
    slot), hands (resid, value, geometry) to a tiny in-process UMD export
    that stores it in TLS, and calls Present() on the same thread.
    `dxgi_present` consumes the TLS slot: the vehicle's DXVK imports the ICD
    frame by resid — the battle-tested alias-import path INCLUDING the
    `6eab004c` copy-time consumer wait, so the published slot orders the
    copy against the ICD's GPU writes for free — copies into
    `hSurfaceToPresent`'s pDrvPrivate DXVK texture, and publishes with its
    OWN fence (correct: the vehicle genuinely wrote the backbuffer). The
    double-publish conflict stops existing — single writer, single
    publisher, gate on the real writing device. Same GPU copy count, zero
    CPU bytes; WSI images become device-local/tiled (no linear cpu_map
    constraint — a rendering win). Remaining engineering: (a) vehicle
    lifecycle off the ICD hot path (the residual-risk contract), (b) the
    UMD export + TLS present-source slot + dxgi_present special case,
    (c) ICD publish of its frame resid (C writer for the seqlock format),
    (d) present-mode mapping, (e) resize/teardown lifecycle. Verify item:
    the ICD's WSI images must be created as shared-importable dedicated
    blobs (wire resources) so the vehicle context can import them. The
    async sw worker stays as fallback wherever the vehicle fails.
    **ROAD 4 IMPLEMENTED END-TO-END (23rd session, 2026-07-06; mesa
    `8a4e331ea9e..737cb2309b3`, dxvk-helios `6069f323`, main `6521d93` +
    `41fa1c4`). Kill switch `HELIOS_WSI_DCOMP_PRESENT`, DEFAULT OFF —
    flip-on is owner-gated (ladder e). Shipped shape:** vehicle lifecycle
    on a dedicated worker (parks after READY and owns the COM release, so
    the nested D3D11→UMD→DXVK→ICD2 teardown never runs on an ICD1 thread);
    frame images stay OPTIMAL buffer-blit images but their dedicated memory
    exports OPAQUE_FD → USE_SHAREABLE blobs; per-chain named present-order
    timeline (`Global\HeliosPresentFence_<pid>_<0x8000_0000|n>` — ICD id
    space is the high-bit half, the UMD producer counter owns the low half)
    signaled on every pre-present submit; seqlock publish from a
    byte-compatible C writer (wsi_helios_present_sync.c); UMD exports
    `helios_umd_set_present_source`/`_wait_last_present` (TLS, same-thread
    contract); dxgi_present special case alias-imports the frame by resid
    (typed identity, per-resid cache — 3 imports per geometry for 30k+
    presents) and copies at the DXVK image level from the LIVE storage
    (staging ALIAS if present; the COM CopySubresourceRegion path would
    read the never-refreshed private image), publishes the backbuffer with
    the vehicle's own fence, no gate, pfnPresentCb as today. Ladder:
    (a) probe PASS 1082 presents; (b) vkcube READY→LIVE, ~64 fps FIFO
    vsync-paced, 30k+ presents ZERO import/copy/geometry/overwrite
    failures, copy-time wait timeouts=0 noslot=0, dwm imports the vehicle
    backbuffers + fence (val tracking live), venus ctx destroyed at exit,
    resize-recreate exercised live (800x600→1896x959); (d) mover-churn
    paintcap clean, WUDFHost timeouts=0, BARRIER=0. **Flip no-copy
    invariant CONFIRMED: every vehicle 'DXGI Present:' has dst=0x0.**
    GOTCHAS: a maximized vehicle chain gets promoted to direct/independent
    flip — correct on the display but ABSENT from GDI-based paintcaps
    (eyeball via Looking Glass; owner-confirmed live); UmdTrace was ON for
    the invariant check — it confounds fps measurements.
    **Doom (immediate, menu): 40 fps v1 → 75 fps** after two measured
    fixes (frame-latency-waitable DROP for non-FIFO — windowed Present()
    otherwise blocks at dwm's compose pace; skip the worker's 4.1 ms
    frame-fence wait while the vehicle serves — ordering is the copy-time
    wait + pre-present throttle). Remaining gap to the 160 sw baseline is
    the image-recycle guard `wait_last_present` (6.1 ms serial,
    present-gate telemetry) stacked on the copy-time wait (8.4 ms
    overlapped): both are venus fence-OBSERVATION latency (ring≥1 retire →
    retire thread → NT signal; ICD2 submission-fence poll), not GPU time
    (~0.3 ms copy). NEXT LEVERS: (1) GPU-side recycle gating — export the
    vehicle's (fenceId, value) back through the UMD, import the named
    fence in ICD1 once, and make image reuse a timeline WAIT at acquire
    instead of a worker-serial CPU wait (fully pipelined; the run-ahead
    absorbs the latency); (2) shave the retire→signal path itself (helps
    every WS1 #4 consumer). sw async worker remains the default and the
    per-frame fallback (the pre-present blit still runs in vehicle mode —
    skip it only with numbers, it is what makes fallback seamless).
    **LEVER 1 IMPLEMENTED (24th session, 2026-07-06; mesa `10f72c50104` +
    `3e5fa9eb1ce`, main `1c34671`; UMD `helios_umd_c78f1a7e01df20b4`, ICD
    `vulkan_virtio-d49cd875438d`).** dxgi_present hands the vehicle's
    (fenceId, value) back via `helios_umd_get_present_result` (TLS,
    same-thread, counted misses/overwrites); the WSI imports
    `Global\HeliosPresentFence_<pid>_<fenceId>` once per chain and gates
    image reuse at ACQUIRE (bound `HELIOS_WSI_VEHICLE_WAIT_US`, 0=off;
    `acquire-gate:` diag telemetry per 512; drops recycle ungated;
    fallback = the old serial wait, `gate_fb` counter; teardown drains max
    pending value). Measured: worker cycle 13→4.4 ms, present-gate serial
    lines GONE, gate avg 2.7 ms overlapped, timeouts 0, gate_fb 0; vkcube
    FIFO ~60 clean; ladder clean (mover, WUDFHost timeouts flat,
    BARRIER=0).
    **CRASH FOUND+FIXED en route (first gated vkcube run froze the guest
    and degraded the whole desktop): vkr resolves vkSignalSemaphore only
    for >=1.2 devices or with KHR_timeline_semaphore enabled at create and
    dispatches it with NO null check** — vn_WaitSemaphores' imported-win32
    post-wait counter sync on a Vulkan 1.0 app (vkcube) = ip=0 segfault of
    the per-context render worker (`journalctl: vkr-ring-<ctx> segfault at
    0`), which the guest NEVER sees: qemu logs only
    `virgl_renderer_context_create_fence: Operation not permitted`
    (proxy_context_submit_fence → dead worker socket → -1), the poisoned
    submission's fence never retires, the app wedges in vkWaitForFences
    and everything else crawls on the backpressure. Fix (mesa
    `3e5fa9eb1ce`): append KHR_timeline_semaphore at device create for WSI
    devices on <1.2 instances (also legitimizes the pre-existing
    present-order timeline signals on 1.0/1.1 apps) + skip the host
    counter sync when the procs cannot exist. dwm/probe never hit it (1.3
    devices). Recovery for a wedged desktop: Stop-Process dwm.
    **OPEN — Doom menu capped at exactly 60.0 fps on BOTH paths today**
    (sw AND vehicle, gate on/off identical, QMP fence-trace on/off
    identical, foreground attempt identical; drops=0, gate timeouts=0,
    worker 4.4 ms, sw wait_avg 3.7 ms — nothing visible adds to 16.6 ms).
    The 22nd/23rd 120/160/75 numbers came from the same unattended
    launcher, so this is a NEW environmental cap, not the acquire gate
    (gate-off A/B proves it). Suspects to disambiguate with the owner:
    idTech background/unfocused throttle state, Steam relaunch settings,
    LGIdd/dwm restart state vs yesterday's boot. Tool:
    `tools/read-vehicle-counters.ps1` samples live minted/drops/gate
    counters from a running process (the perf line never prints on
    pure-vehicle runs); `Z:\tmp\movewin_target.txt` + helios_movewin
    foregrounds an arbitrary window title. Owner gameplay: ~70-80 fps.
    **STALE-FRAME STUTTER FIXED (24th session, cont.; owner-confirmed
    "much better"; main `5a9d5d5`).** Root cause: direct/independent-flip
    presents (Doom's near-fullscreen window) are ordered only on the KMD's
    decode-complete DMA fence — the backbuffer scanned out before the
    venus copy landed, showing the buffer's 3-presents-old occupant. dwm's
    consumer wait protects only COMPOSED presents. Fix: vehicle presents
    now run the frame-completion gate on the WORKER before pfnPresentCb
    (`VehicleFlipGateUs`, default 32 ms, 0=off; 6.7 ms avg = the copy's
    fence-observation latency; acquire-gate wait dropped 2.7→1.7 ms).
    Pipelined follow-up: dxgkrnl WaitForSynchronizationObjectFromGpu on
    the present packet + the producer fence before the copy flush (also
    retires the copy-time CPU wait for vehicle copies). Related cleanup
    (dxvk `7255c1e6`): the redundant pre-6eab004c list-start consumer wait
    in refreshHeliosStagedImages removed for the alias variant.
    **THE 200-fps LEVER FOUND — escape-parked fence waits convoy submits
    (mesa `fbf38f4cc63` has the telemetry + stopgap).** Phase splits
    ([prep/ring/win32] on QueueSubmit2, [mutex/escape/sync] on
    helios_submit) proved: the pre-present submit's 2.5-2.9 ms is entirely
    the win32-signal SUBMIT_VENUS D3DKMTEscape, which serializes at the
    dxgkrnl escape layer behind the retire thread's blocking WAIT_FENCE
    escapes (parked up to 250 ms/slice; processes with no parked waits —
    dwm, the sw path — submit at 3-5 µs). Slice 250 ms → 2 ms bounded the
    convoy (escape 2.9 → ~1.0-1.6 ms) and CONFIRMED the mechanism; Doom
    stays ~60-70 because its OWN vkWaitForFences ride the same parked
    escapes. NEXT SESSION (the fix): **KMD-signaled usermode fence
    events** — non-blocking REGISTER_FENCE_EVENT escape (fence_id, event
    HANDLE), KMD DPC KeSetEvent at wire-fence retirement (in-flight table
    already has ISR→DPC), retire thread + helios_wait park on the EVENT
    in usermode. Kills the convoy class entirely AND collapses the
    fence-observation latency every consumer pays (app fence waits, flip
    gate, acquire gate, retire→signal chain). KMD unit: event table with
    ObReferenceObjectByHandle lifetime + process-teardown cleanup +
    version bump; ICD unit: register/park/cancel in helios_ioctl_wait_fence
    path. Deployed at session end: UMD `helios_umd_89e95497a7d6d08f`, ICD
    `vulkan_virtio-2900b457e859`, dxvk `7255c1e6`, mesa `fbf38f4cc63`.
    **USERMODE FENCE EVENTS SHIPPED (25th session, 2026-07-06; main
    `16da0eb` = KMD 22.22.54, mesa `e5f35c18bf9`; deployed ICD
    `vulkan_virtio-9384eb059a8f`, UMD unchanged `89e95497a7d6d08f`).**
    REGISTER/UNREGISTER_FENCE_EVENT escapes: PASSIVE
    ObReferenceObjectByHandle → bounded (fence_id → PKEVENT) table;
    retirement DPC KeSetEvent + ObDereferenceObjectDeferDelete; one-shot;
    already-retired = atomic check under the device spinlock (no lost
    wakeups); capability probe (fence_id=0/handle=0 → PROBE_ACK, old KMD
    → NOT_IMPLEMENTED → loud diag + blocking-escape fallback). ICD:
    per-thread tss event, register → WaitForSingleObject → cancel;
    UNREGISTER NOT_FOUND + signaled event = raced (complete), +
    unsignaled = teardown purge (loud, INCOMPLETE). Retire thread: 2 ms
    slice stopgap retired from the event path — one WFMO on {fence
    event, retire_stop_event}, 60 s deadline; slice loop survives only
    as the old-KMD fallback. All counters in QUERY_STATS v2 + a
    `fence_events` perf-summary line.
    **BEFORE/AFTER (Doom vehicle, unit-0 baseline 23:00 → post-deploy
    23:35): submit_phases escape 465-786 µs → 111-126 µs; QueueSubmit2
    [prep 11.2/ring 45.5/win32 351 µs] → [5.2/2.3/87 µs] (submit_avg
    390 → 89 µs); acquire-gate (vkcube) 2.6-2.9 ms avg max 11-21 ms →
    196-273 µs avg max <1 ms; fence_events waits=2455 imm=245 raced=0
    timeouts=0 fallbacks=0 lost=0; vkcube FIFO ~60 clean; WUDFHost/dwm
    timeouts 0, BARRIER 0.** Gates passed: escape µs-class ✓, win32
    <100 µs ✓. PENDING: owner Doom gameplay fps + stale-frame stutter
    eyeball on the new stack (baseline gameplay flip-gate max was
    20-29.7 ms = the 2-3-vblank hitch signature; event waits should
    collapse it), present/flip-gate gameplay averages, ladder e soak.
    **DOOM LEVEL-LOAD FATAL ROOT-CAUSED + FIXED (same session; main
    `aedd8ba` = KMD 22.22.55): "Cannot map buffer with usage BU_STATIC"
    = MAP_BLOB → STATUS_INSUFFICIENT_RESOURCES = the adapter-global
    user-VA MappingTable still sized to the ORIGINAL MAX_BLOBS (256)
    after blobs grew to 8192** — desktop held ~223 mappings, the level
    load's vkMapMemory burst blew the remaining ~33 (probe-proven:
    map-and-hold refused at held=33 with 0xC000009A; size sweep 1-256
    MiB all passed, falsifying the MDL-CSHORT hypothesis; failure
    signature present at 05:28 pre-change = NOT a fence-event
    regression). Fix: MAX_MAPPINGS 8192 + the three indistinguishable
    0xC000009A refusal sites individually counted (mapping-full /
    map-pages / window-alloc) in QUERY_STATS v2.
    `tools/blob_map_size_probe.c` = size sweep + concurrent-headroom +
    v2 stats reader (gcc on the VM, no vcvars needed).
    **STALE-FRAME A/B VERDICT + KERNEL-ENFORCED FLIP ORDERING PROVEN
    (25th session cont., 2026-07-07; main `77dffe2`).** Owner A/B on the
    fence-event stack: **sw path 200 fps (was 160 — the event waits
    bought sw +40 fps) with ZERO stale frames; vehicle path 120-130 fps
    with stale-frame stutter still present** → the leak is the
    vehicle/UMD flip path, pre-existing, fps-scaled. Counted leak sites
    that window: flip gate 11 timeouts + acquire gate 3 timeouts per
    ~41k presents (each "proceeds loudly" = a stale flip candidate);
    fence-event machinery itself clean (0 fallbacks/0 lost). NEXT ROAD —
    **kernel-enforced vehicle flip ordering** (replaces the bounded CPU
    worker gate): queue D3DKMTWaitForSynchronizationObjectFromGpu on the
    copy's monitored fence AHEAD of the present packet, so dxgkrnl holds
    the flip until the retire thread's CPU signal lands — no usermode
    cap to leak, and the 6.6 ms worker serial gate retires (fps win).
    `tools/vehicle_flipwait_probe.c` PROVES the primitive live on our
    software-scheduled adapter (queued signal held behind an unsatisfied
    wait, drained ~10 ms after the CPU signal; ZERO KMD changes) and the
    topology: raw cross-device sync handles are REJECTED 0xC000000D —
    the fence must be NT-shared (D3DKMTShareObjects) and reopened via
    OpenSyncObjectFromNtHandle2 on the device owning the waiting context
    (the WS1 #4 named-share consumer pattern). Implementation sketch:
    UMD opens the vehicle fence (fence_id known from the present result)
    on the device owning `dev.h_context`, queues the wait right before
    pfnPresentCb; CPU gate kept as fallback knob + wedge watchdog
    (a never-signaled fence would park the context queue). Second half
    (same primitive): producer-fence wait before the copy flush retires
    the 7 ms copy-time CPU wait. Ladder: wait-honored telemetry → flip
    gate CPU path off → owner stutter eyeball → fps before/after.
    **IMPLEMENTED + WEDGED (main `e9cdae6`, default OFF; VM registry
    VehicleKernelFlipWait=0 guards the deployed default-ON UMD
    `fb564d5072f8a58e`).** First live test (vkcube): present #1 armed +
    queued OK, but the enqueueWait signal NEVER fired (exported present
    fence counter never read ≥1 in the producer's own process — likely
    an untested vn_GetSemaphoreCounterValue/vn_WaitSemaphores
    exported-win32 path), the watchdog unwedge released the flip but the
    app never presented again (the WSI worker parks on something after
    pfnPresentCb — find it), and TerminateProcess on the wedged instance
    HUNG THE ENTIRE GUEST — no bugcheck, no dump (kernel deadlock;
    dxgkrnl/KMD teardown of a context with a parked queue + queued
    monitored-fence wait whose signal source died). QMP system_reset
    recovered. **26TH-SESSION TOP PRIORITY: root-cause + fix all three
    (see the flip-kwait-wedge memory: NMI-crash/live-KD recipe, suspect
    KMD teardown paths, retire-thread-signal design fallback).**
    **26TH SESSION (2026-07-07): DEFECT (a) ROOT-CAUSED + FIXED (mesa
    `b2f47c780d2`, deployed ICD `vulkan_virtio-e31ec528ac79`); the
    GUEST "HANG" CLASS SOLVED — it was the SERIAL KERNEL DEBUGGER.**
    (a) The producer's EXPORTED present fence was unobservable in its
    own process: `is_external` skips the feedback slot, the queue
    signal routes out-of-band onto the helios_sync/WDDM fence (retire
    thread only), and both vn_GetSemaphoreCounterValue (host ring
    round-trip: 0 forever) and vn_WaitSemaphores (win32 fast-path was
    imported-only) never read the WDDM fence. Fix: fold
    vn_renderer_sync_read into the counter read for ANY win32-backed
    payload + widen the wait fast-path (event wait); host-counter sync
    stays imported-only (exported host object has a pending GPU signal
    op). Verified: knob=1 vkcube presents #1-2 armed AND RELEASED
    (was: #1 wedged); first-rescue diag `wddm=852/1307 host=0`.
    (c) The whole-guest freeze reproduced WITHOUT any kill (knob=1
    vkcube after ONE wedge+watchdog-unwedge cycle, ~3 min fuse) —
    QMP `inject-nmi` → bugcheck 0x80 minidump `070726-4890-01.dmp`
    (copy in Z:\tmp): 15/16 vCPUs in nt!KiFreezeTargetExecution, CPU#6
    polling kdcom!READ_PORT_UCHAR — the guest had dropped into the
    SERIAL KERNEL DEBUGGER (bcdedit debug=Serial port 1, NO kdcom
    client exists — ntoseye is a gdbstub) and froze forever. With KD
    enabled, dxgkrnl asserts / the ERESOURCE deadlock detector / TDR
    BREAK instead of bugchecking — that is why neither hang ever left
    a dump. TerminateProcess was never the root cause.
    (b) Caught red-handed in the same dump (stack scavenge of the
    frozen vkcube thread): a runtime thread inside
    DxgkWaitForSynchronizationObjectFromGpu blocked MINUTES in
    nt!ExpWaitForResource on a dxgkrnl ERESOURCE (holder unknown —
    triage dump has one stack) until nt!ExpResourceTimeoutCaptureLiveDump
    → KiBreakpointTrap → KD entry. So the worker's forever-park after a
    flip-kwait stall = a dxgkrnl-internal ERESOURCE convoy seeded by
    the stalled flip, not a WSI wait (all WSI waits audited bounded).
    RESIDUAL OPEN: present #3's wire fence never retired (fence stalled
    at 2 with 3 queued; #1-2 clean) — the remaining true stall; and the
    WSI perf-counter oddity (presents=0/ready=0 while 3 vehicle
    presents + gate-ARM demonstrably ran). TOOLKIT NOW ARMED:
    CrashDumpEnabled=2 (full kernel dump next time → !locks names the
    ERESOURCE holder); PROPOSED: bcdedit /debug off so every future
    freeze self-converts to dump+reboot (testsigning + ntoseye/gdbstub
    unaffected). Registry knob back to 0; ICD fix deployed + desktop
    cold-verified healthy on it.
    **26TH SESSION CONT.: THE DEADLOCK CLASS KILLED — ERESOURCE HOLDER
    NAMED AND FIXED (mesa `2cc63d82468`, deployed ICD
    `vulkan_virtio-178211300d5f`; bcdedit /debug off applied).** The
    owner-approved second NMI (full kernel dump, CrashDumpEnabled=2)
    caught it red-handed: the exclusive holder of all 3 contended
    ERESOURCEs was the process's own next VENUS ESCAPE —
    HardwareAccess=1 escapes run dxgkrnl
    AcquireCoreResourceExclusive → DXGPROCESS::FlushAllDevice →
    VidSchWaitForCompletionEvent, i.e. they WAIT FOR EVERY CONTEXT
    QUEUE TO DRAIN while holding the core resource; with a kwait-parked
    queue whose signal comes from user mode, every
    SignalSynchronizationObjectFromCpu (dxvk waiter, watchdog — user
    dump caught the watchdog parked IN the signal syscall, so the
    25th-session "unwedge released the flip" was FALSE — and the ICD
    retire thread) convoys behind it = deadlock; wedge point #1/#2/#3
    varied by race. Fix = unit 3's HardwareAccess=0 (correctness, not
    just perf; `HELIOS_ESCAPE_HW=1` env kill switch) + the exported-sem
    fold + a signalTo monotonicity guard (main `6d41f8e`, deployed UMD
    `helios_umd_3b4a9e66394e9523`). **LADDER RESULTS (2026-07-07
    02:45-02:53): 24,064 kwait presents, 0 wedges / 0 arm fails /
    0 queue fails / 0 signal fails; mid-run kill (owner) → clean
    teardown, no zombie, guest healthy. BUT THE OWNER EYEBALL RUNG
    FAILED: vkcube showed the Doom-class STALE-FRAME STUTTER from
    ~40 s in — kernel flip ordering alone does NOT close the stale
    class. Plus a new observation: a freshly launched (unfocused)
    vkcube alternates between TWO stale frames until the window is
    clicked, then rendering progresses. DEFAULTS STAY OFF
    (VehicleKernelFlipWait code default OFF, registry knob back to 0).**
    **OWNER CONFIRMED: both issues are ABSENT on the sw present path**
    — they are properties of the dcomp VEHICLE presentation layer, not
    of fences/venus in general (matches the A/B: sw 200 fps zero
    stale). 27th-session hypotheses, vehicle-layer-first:
    (c1) DXGI_STATUS_OCCLUDED — Present() on an occluded/background
    window returns a SUCCESS status (0x087A0001) and does not display;
    wsi_win32_queue_present_vehicle checks only FAILED(hr)
    (wsi_common_win32.cpp:1985-1995) so occluded presents "succeed"
    silently → add per-present hr!=S_OK logging (cheap, likely explains
    the click-gated launch behavior); (c2) dwm/dcomp consumption pacing
    for background visuals, direct-flip vs composed transitions;
    (a) backbuffer clobber — the venus copy lands at Present-call time
    while up to frame-latency flips sit parked, so rotation may
    overwrite a buffer whose flip has not scanned out (instrument
    backbuffer ptr + flip completion pacing; consider bounding armed
    depth); (b) U:=V fires at ring-fence retirement measured at 97-98%
    of T_gpu — check the tail. First step: knob=0 vehicle A/B of the
    unfocused-launch behavior + the hr logging.
    **27TH SESSION (2026-07-07): (c1) FALSIFIED + full static analysis of
    the HW present path.** hr-transition/drop-streak instrumentation
    deployed (mesa `e3d2fdf61c1`, ICD `vulkan_virtio-1545839fc535`;
    `odd_hr` counter in the perf line; paintcap_hidden schtask = capture
    without the console occluder/focus steal): under a 45 s full-screen
    occluder the knob=0 chain presented at a steady 60 Hz, hr == S_OK
    throughout, acquire-gate timeouts 0 — composition swapchains never
    report OCCLUDED and dwm keeps consuming occluded chains; presents
    flowing says NOTHING about display. Owner clarified the property set:
    launch alternation + random self-healing stutter are both cured by ANY
    rendering activity (notepad open — no focus change!) ⇒ the lever is
    activity, not focus. Static analysis (3-leg map, agent reports in the
    session transcript): every vehicle ordering protection is a bounded
    32 ms wait that LEAKS stale on timeout (consumer copy-wait
    dxvk_context.cpp:9507 "copying anyway"; flip gate forward.rs:5902
    "flipping anyway"; acquire release-gate wsi_common_win32.cpp:1683
    proceed; dwm-side staged-refresh wait ditto) — all funnel into named-
    fence advancement by the per-process ICD retire thread, whose event
    wait is the ONE chain link with NO self-heal: KMD fence events are
    interrupt-edge-driven (drain_used from the ISR/DPC or opportunistic
    drains on the process's OWN escape traffic only; no KMD timer;
    register-vs-retire race proven closed, gpu.rs:1317/escape.rs:186), so
    a lost/deferred INTx for a mostly-IDLE process (dwm on a static
    desktop!) parks its retire thread ≤60 s with nothing to kick it.
    PRIME SUSPECT (fits all 4 properties + why vkcube-side counters were
    all clean): the stall is in DWM's process — dwm consumes the vehicle
    backbuffers through the alias-staging refresh + 32 ms consumer wait;
    a parked dwm retire thread ⇒ dwm composes stale staged copies of the
    2 ever-refreshed backbuffers (= the TWO alternating frames) while the
    app's flips rotate healthily; dwm idle ⇒ few escapes ⇒ no drains ⇒
    self-sustaining until any dirty region makes dwm submit (notepad,
    click) ⇒ drain ⇒ unstick. Secondary: retire-thread serial FIFO
    convoy (one stuck head delays every later signal = stutter burst,
    self-heals = P3/P4); vehicle named release fence created LAZILY
    inside present #1 (dxvk_bridge.cpp:1434) + consumer import-failure
    negative-cache of 256 LOOKUPS (dxvk_context.cpp:9482) = long
    unordered window at chain birth. SOLUTION CHOSEN (owner: NO HACKS):
    prove the failing link with ONE owner repro, then fix that link at
    its ROOT — (a) event registered-but-never-signaled + INT_ROUTINE
    stalled ⇒ interrupt delivery ⇒ MSI-X (MSISupported=0 is a bring-up
    leftover); (b) used entry never posted ⇒ submission/doorbell path;
    (c) event signaled but fence behind ⇒ ICD retire logic. The sliced-
    polling retire wait + KMD timer backstop are REJECTED as hacks;
    eager vehicle fence + time-based import retry are clean but staged
    AFTER the repro so they cannot mask it. Evidence kit already
    complete: dwm_helios_umd_dxvk.log present-wait lines (import lines
    prove the channel live; ZERO timeout lines in healthy runs),
    helios_icd_diag.log retire "GIVING UP ... stays UNSIGNALED" lines
    (PROVEN to fire — 2 instances 2026-07-06), QUERY_STATS v2
    FENCE_EVENT_*/INT_ROUTINE via tools/blob_map_size_probe.c,
    paintcap_hidden content diffs. REPRO IS OWNER-ONLY: schtasks
    launches always take foreground (fg=1 across 4 steal attempts), and
    automated probing IS the curing activity (self-defeating). Owner
    recipe: reproduce the alternation, hands off ~90 s (>60 s arms the
    GIVING-UP diag), note the time; then read the four channels.
    **27TH SESSION VERDICT + FIX DEPLOYED (KMD 22.22.56, main `1a07814`).**
    Owner repro delivered the discriminator: during a LIVE two-frame
    alternation (~60 s, hands-off recovery) EVERY sync in the DAG was
    green — vkcube 60 fps/acquire-gate 64 µs/0 timeouts, vehicle 29k
    copies 0 fails/flip-gate 390 µs, dwm waits 7.9 ms 0 timeouts 0
    noslot on the live chain, WUDFHost 6.3 ms 0/0 — so the fence class
    was EXONERATED (retire/interrupt fixes rejected as the wrong tree).
    Second audit pair: our flip-emulation identity rotation is provably
    self-consistent (copy dst == flip src per present; storage+identity
    rotations = same permutation; publish resid tracks rotation; no
    in-tree pairing skew), BUT the caps surface lied:
    `DXGK_DRIVERCAPS.SupportDirectFlip=1` (NO bisect provenance — never
    load-mandatory) + aperture-segment DirectFlip flags, on an adapter
    with ZERO scanout displaying through IddCx-captured COMPOSITION.
    dwm promotes the eligible dcomp vehicle visual (flip-model +
    IGNORE-alpha + unoccluded) to direct/independent flip and STOPS
    COMPOSING it — two alternating stale frames = dwm's last composed
    pair; any dirty-region recompose demotes it (taskbar clock minute
    repaint = the hands-off ~60 s recovery; console-overlapped schtasks
    launches never repro'd because occlusion kills eligibility; the
    23rd-session "maximized chains vanish from paintcaps" was the same
    promotion). The UMD already denied CheckDirectFlipSupport — the KMD
    now agrees: SupportDirectFlip + the 3 aperture DirectFlip flags are
    DENIED by default behind the `DirectFlipCaps` service knob (0 =
    deny, 1 = legacy A/B via reg add + devcon restart; state in diag
    0x01D7 bit 2, DiagLevel-gated). Display-less-adapter guidance
    (mcdm-implementation-guidelines.md) mandates 0. DEPLOYED + cold-boot
    verified: 22.22.56 CM_PROB_NONE, release UMD re-pinned
    (helios_umd_3b4a9e66394e9523; devcon reset it to debug — known
    trap; DriverStore copy sync failed file-in-use, cosmetic), desktop
    composited healthy. OWNER VERDICT: NO CHANGE — direct-flip denial
    falsified as the mechanism (kept as the truthful cap surface).
    **TRUE ROOT CAUSE FOUND + FIX DEPLOYED (dxvk `1cdf0837`, mesa
    `10cefe67c64`; UMD `helios_umd_746b0242cf664825`, ICD
    `vulkan_virtio-390d1b583dba`).** Discriminating evidence: owner
    repro with per-window counter sampling — during a LIVE dance dwm
    performed ZERO consumer reads for 20+ s while composing 60 fps
    (waits/fast/noslot all frozen), and mouse movement cured it
    INSTANTLY (both dance and stutter) — consumer freshness tracked
    dxvk COMMAND-LIST CADENCE, not producer progress: staged imports
    re-stage only at list starts, an idle dwm's CS chunks span many
    frames (~60 s to fill when idle = the hands-off recovery), so its
    sampled staged copies froze while every fence stayed green; the
    front-buffer rotation cycled 2-3 differently-aged frozen copies =
    the two-frame dance; chunk-threshold oscillation = the stutter;
    activity = per-frame flushes = cure. sw path immune because
    GdiAccelMode=0 routes GDI content via CPU redirection surfaces
    (not our staged imports). THE FIX (dxvk-helios, 3 parts):
    (1) bind-time staleness gate — staged-SRV binds arm a sticky
    per-context flag; draws/dispatches on the immediate context compare
    each bound staged image's published present-sync value against the
    value its last re-stage observed and Flush() when the producer
    advanced (≤1 flush per published value per image via a claim CAS;
    non-consumer processes pay one branch per draw); (2) zombie-refresh
    unenroll — texture dtor flags the image, the refresh loop erases it
    (was: 15k+ no-slot full-image copies per DEAD vkcube chain until
    the 3600-tick prune, Rc-pinning the corpse's venus resources);
    (3) present-sync slot recycling by PRODUCER CREATION TIME
    (reserved2 repurposed; pid liveness alone let cross-boot pid reuse
    keep 64/64 slots stale — publishes were being DROPPED table-full).
    Along the way: 366-368-vs-389-391 resid "mismatch" resolved as two
    chain generations (live-chain publish/lookup rendezvous verified
    healthy, 3 rotating slots advancing per frame). AWAITING owner
    verdict on the fix. Then: ladder rungs (Doom fps vs 120-130,
    kwait default decision, HELIOS_WSI_DCOMP_PRESENT default), and the
    deferred cleanups: per-present sw-fallback insurance blit on
    vehicle chains ("measure before removing"), in-process present-sync
    slot/named-fence round-trips (set_source carries a dead
    fence_value), eager vehicle fence at init, time-based import
    negative cache, DirectFlipCaps knob retirement decision.
    **FIX CONFIRMED (owner: "working very well"); 28TH SESSION =
    WINDOWED DOOM PERF.** Doom telemetry (pid 9444, 1880x943 windowed
    vehicle, immediate/tearing=1, ~105 fps): flip gate avg 5.57 ms
    (worker-serial) + acquire gate avg 4.06 ms (app), both timeouts=0 —
    pure fence-observation latency of the vehicle copy;
    queue_present_avg 5.96 ms. FULLSCREEN "200 fps rock-stable" = THE
    SW PATH: the fullscreen (1896x1030) chain's vehicle build FAILED at
    stage='dcomp target/visual' hr=0x88980800 and latched → sw direct
    path at ~0.85 ms/frame CPU (creates=2 fails=1 in
    helios-doom-wsi-perf.txt). NEW DEFECT: vehicle re-create for the
    same hwnd fails (one-dcomp-target-per-hwnd on resize/fullscreen;
    vkd3d likely creates a NEW VkSurface for the same hwnd → per-surface
    target cache misses → second CreateTargetForHwnd fails). dwm
    post-fix: noslot=54 (was 68k), fence_events 0 fallbacks/0 lost,
    escape 64 µs. Doom perf files: C:\Users\Rupansh\helios-doom-perf.txt
    + helios-doom-wsi-perf.txt (owner's launcher tees them).
    **28TH SESSION: the dcomp target re-create defect FIXED (mesa
    `bbf5e33f314`, ICD `vulkan_virtio-437986e5fcc4` deployed).** One
    composition target per hwnd is a Windows rule; the target/visual
    cache was per-VkSurface, so a new VkSurface for the same hwnd
    (vkd3d resize/fullscreen) failed CreateTargetForHwnd 0x88980800 and
    latched onto the sw path. Fix: process-global refcounted
    hwnd→target registry under the vehicle runtime mutex; the visual's
    content owner (current_swapchain) lives on the shared entry so a
    retired chain on the OLD surface cannot blank the new chain's
    content; `tgt_reuse` counter added to the perf line. Proven by
    tools/vk_surface_recreate_probe.cpp (schtask `helios_vk_recreate`,
    session 1, time-based phases — frame-count phases finish before the
    ~5 s async vehicle build, first attempt exercised nothing): chain A
    LIVE holding the target → chain B (new surface, SAME hwnd, A alive)
    READY+LIVE → chain+surface A destroyed under live B, B presented on
    (acquire-gate 0 timeouts). The honest fullscreen-vehicle Doom A/B
    is now unblocked (owner run — expect creates=2 fails=0 and the
    fullscreen chain LIVE instead of the 0x88980800 latch).
    **28TH SESSION cont. — kwait rung 1 GREEN + copy-side wait FALSIFIED
    + both cleanup counters live (mesa `06e27a05ea3`, dxvk `7c82271f`;
    deployed ICD `vulkan_virtio-43feb2709167`, UMD
    `helios_umd_801a8571aff69c67`, `VehicleKernelFlipWait=1` in the
    registry).** (a) Kernel flip-wait smoke: vkcube dcomp 4608/4608
    presents kwait_armed, 0 arm/queue fails, acquire-gate 66 µs avg
    0 timeouts, 40 s no wedge, content advancing in paintcap diffs —
    the 5.6 ms worker-serial flip gate is retired whenever the knob is
    on; Doom A/B vs the 105 fps baseline = OWNER RUNG (knob already
    live). (b) The 25th-session "producer-fence wait before copy flush"
    idea is FALSIFIED as a lever: Doom's 28k-present run logged ZERO
    copy-time consumer waits (no present-wait/unordered/noslot lines in
    umd-9444.log) — the copy-time CPU wait always fast-paths; the
    4.06 ms acquire gate is copy COMPLETION+OBSERVATION latency, not a
    CS-thread wait. Not built (measure-first). (c) Insurance blit:
    HELIOS_WSI_INSURANCE_BLIT=0 skips the per-present image->buffer
    fallback blit on vehicle-serving chains (insurance_skipped counter
    in the common perf line; vkcube A/B clean, 2780 skips, content
    correct) — default ON until the Doom A/B numbers land. (d)
    dwm-side staleness-gate cost now visible: gate_flushes in the
    present-wait line — ~50 flushes / 13.7k consumer reads (~0.4%) on
    a live desktop, the 27th-session fix is cheap. OWNER LADDER: Doom
    windowed (kwait live) fps + gates vs 105; fullscreen re-try
    (target-registry fix) — expect vehicle LIVE, honest fullscreen
    number vs the 200 fps sw path; optional HELIOS_WSI_INSURANCE_BLIT=0
    leg; stale class must stay dead throughout (vkcube shows no
    regression). THEN: kwait + DCOMP default decisions, residual
    stutter triage (105-vs-60 Hz judder, gate max-spikes).
    **OWNER DOOM VERDICT (same-process windowed→fullscreen, kwait=1 +
    insurance=0): no fps change; stutter still present WINDOWED, GONE
    FULLSCREEN.** Evidence (pid 6220): the fullscreen 1896x1030 chain
    went VEHICLE (READY+LIVE on the same hwnd as the windowed chain —
    the target-registry fix confirmed in the wild); kwait_armed
    6144/6144, 0 arm/queue fails, no wedge; insurance_skipped
    13176/13200; queue_present_avg 5.96→2.81 ms (flip gate retired) BUT
    acquire-gate 4.06→7.69 ms avg (max ~20 ms, timeouts 0) — the
    latency the worker gate used to absorb moved to acquire: the fps
    limiter is the vehicle copy's COMPLETION+OBSERVATION latency
    (~1 host-GPU frame at saturation), not CPU gates. Gates are now
    honest pipeline measurements, nothing left to delete guest-CPU-side.
    STUTTER LOCALIZED: same process/gates/fps, stutter only when dwm
    COMPOSES the chain (windowed); fullscreen (no dwm compose leg)
    smooth ⇒ the stutter lives in dwm's consumption leg. dwm telemetry
    during the run: gate_flushes 2480 (~60/s — the freshness gate does
    per-frame work under a game, expected) and consumer waits avg
    8.9 ms when they fire (~8% of reads) — 9 ms stalls ON DWM'S CS
    THREAD = composition hitches. HYPOTHESIS (next session): the
    staged-refresh loop re-stages ALL enrolled vehicle backbuffers at
    list start, including the just-presented one whose copy is still
    in flight — dwm waits ~9 ms for a buffer it is not composing this
    frame. Fix shape (no hacks — matches real-hw dwm semantics of
    composing the newest COMPLETE frame): skip-if-unretired in the
    refresh (keep the current staged bytes, let the bind-time gate
    force the re-stage next list) instead of the bounded 32 ms wait;
    kwait guarantees the flip itself never outruns its copy, so
    skipping cannot resurrect the stale class. DEFAULTS PROPOSAL:
    kwait code-default ON (Doom+vkcube green, deadlock class dead);
    insurance knob keep (no measurable cost either way at Doom res —
    the copy hides under GPU latency); DCOMP default AFTER the dwm
    stutter fix.
    **STUTTER FIX IMPLEMENTED + DEPLOYED (dxvk `6bcbd282`, main
    `83f9697`; UMD `helios_umd_b70f3e5b23cc5e03`): skip-if-unretired
    staged refresh for kwait-ordered publishes.** Producers advertise
    kernel-held flips in the present-sync slot (fenceId bit 30 — free
    in both id spaces; UMD sets it on vehicle publishes when
    flip_wait_setup succeeded, never on dwm→IddCx publishes; mesa
    publishes never set it); the consumer refresh keeps its current
    staged bytes for an unretired kwait value (that image cannot be
    the sampled front buffer — dxgkrnl holds its flip), re-arms the
    bind-time gate, counts (refresh_skips in the present-wait line;
    dxvk.heliosSkipUnretiredRefresh default ON = kill switch).
    heliosProducerFence factors the import cache; VehicleKernelFlipWait
    CODE DEFAULT NOW ON (registry 0 = kill switch). VERIFIED LIVE:
    dwm skips ~60/s on vkcube's 3 backbuffers with ZERO blocking waits
    since restart and content advancing (paintcap pair); WUDFHost
    refresh_skips=0, waits unchanged 5.8 ms/0 timeouts (the IddCx
    orderer untouched); vkcube kwait 4096/4096 armed via the code
    default. AWAITING owner windowed-Doom eyeball: the 8.9 ms dwm CS
    stalls are gone — stutter verdict decides the DCOMP default next.
    **OWNER VERDICT: STUTTER FIXED (fps ~same, as expected — the
    limiter is copy completion+observation, a separate lever). DCOMP
    DEFAULT FLIPPED ON (mesa `f5037d701ad`, ICD
    `vulkan_virtio-d6e68f7a3322` deployed): the vehicle now serves
    every Vulkan swapchain by default; HELIOS_WSI_DCOMP_PRESENT=0 =
    per-process kill switch. Verified: env-free vkcube goes vehicle
    LIVE, kwait 1536/1536 armed, acquire-gate 84 µs / 0 timeouts,
    content advancing (paintcap pair). The vehicle class (WS2 road 4)
    is DONE: kwait ON by default, skip-if-unretired ON by default,
    hwnd→target registry, insurance knob available. REMAINING WS2
    PERF LEVER: the vehicle copy's completion+observation latency
    (acquire gate ≈ 7.7 ms under Doom ≈ 1 host-GPU frame) — measure
    host-side scheduling before touching. Deferred cleanups: in-process
    slot round-trips (dead set_source fence_value), eager vehicle
    fence at init, WSI perf-counter oddity, cold-boot re-verify,
    GdiAccelMode retirement, DirectFlipCaps knob retirement.**
    **29TH SESSION — COPY-LATENCY ROOT CAUSE FOUND: QEMU delivers venus
    wire-fence completions by POLLING (10 ms fence_poll timer + an
    opportunistic poll on every guest ctrl-queue kick); the async
    fence-callback path is NOT active in the running config.** The whole
    stack below it is fast. Measurement chain (all landed, deployed):
    (a) `copy-lat` in the umd bridge (publish→waiter-observation of the
    vehicle copy fence; umd log, every 512): vkcube on an IDLE GPU avg
    ~13.6 ms, dominant bucket 10-20 ms — the Doom 7.69 ms acquire gate is
    just the tail of this past the app's natural slack. (b) `retire_lat`
    in the ICD (submit→wire-fence-retirement-observed, HELIOS_PERF
    summary + histogram): BIMODAL — ~half <1 ms, ~half 10-20 ms, max
    ≈11 ms (vehicle) / ≈16 ms (producer), RATE-INDEPENDENT (260 fps
    immediate-mode vkcube: slow mode persists and stacks, max 28 ms).
    (c) Host driver EXONERATED: tools/vk_fence_wake_probe.c (Linux,
    empty vkQueueSubmit+WaitForFences on the idle NVIDIA queue) =
    0.23 ms avg / 0.48 ms worst. (d) HOST-SIDE PROOF from the 07-06
    traced run in /tmp/helios-qemu-stderr.log (virtio_gpu_fence_ctrl/
    resp): a lone in-flight fence gets its response +10.5 ms; two
    fences submitted together both complete fast (the second submit's
    handle_ctrl kick polls the first through) — the exact
    poll+kick signature of hw/display/virtio-gpu-gl.c
    virtio_gpu_gl_handle_ctrl → virtio_gpu_virgl_fence_poll and the
    10 ms fence_poll timer. QEMU 11.0.1 enables async fences only when
    `qemu_egl_display` is set + virglrenderer ≥1.1.2 (both look
    satisfied: egl-headless display, virgl 1.3.0, callbacks v4) — WHY
    it is not active is the open host-side question (verify via gdb
    `print qemu_egl_display` on the live process or a traced relaunch:
    `--trace 'virtio_gpu_fence_*'`). FIX DIRECTION (owner decision, VM
    relaunch territory): get VIRGL_RENDERER_ASYNC_FENCE_CB active —
    expected win: every venus fence wait in the system (vehicle copy
    observation, dwm consumer waits avg 8.9 ms, IddCx waits 5.8 ms,
    D3D11 fence waits) drops from ~5-15 ms to sub-ms; the ~105 fps
    windowed Doom ceiling should lift substantially. Vkr map (host,
    for later levers): per-context worker PROCESSES, no cross-context
    CPU locks; per-guest-queue host VkQueue, family passed verbatim;
    ring≥1 retirement = empty marker submit + per-queue sync thread;
    VK_EXT/KHR_global_priority plumbed end-to-end; transfer-family
    queues map 1:1 (both usable if GPU-side contention ever becomes
    the limiter — it is NOT today). Waiter named: the vehicle device's
    `wait_calls fast=0 timeout≈2571` = DxvkFence::run() (dxvk_fence.cpp
    10 ms slice loop) servicing present_flip_wait_arm enqueueWaits —
    slices are benign event-backed re-loops. New tooling: schtask
    `helios_vkcube_imm` (immediate-mode cube, perf files
    vkcube_imm_*); helios_vkcube now also sets HELIOS_PERF →
    vkcube_renderer_perf.txt.
    **29TH SESSION cont. (owner-collaborative) — REFINED + WORKAROUND
    SHIPPED: feedback-shadow retire (mesa `b578e7d42a3`, ICD
    `vulkan_virtio-32e87a6fc919` deployed, device restarted, desktop
    verified).** Refinements from live host debugging (owner gdb/strace/
    sysctls): the async fence path IS configured (proxy flags 962);
    worker retires fences in 50-150 µs (strace); the io_uring fdmon
    theory was FALSIFIED (kernel.io_uring_disabled=2 + reboot: epoll
    shows kick→IRQ p50 33 µs yet guest retire_lat unchanged); vkr ring
    thread + guest KMD interrupt handling audited clean (spec + Linux
    virtgpu cross-checked by the owner; real non-10 ms inefficiencies
    noted: INTx shares IRQ 22 with the balloon → KMD MSI-X someday;
    serial retire-thread waits). Guest no-WSI probe
    (tools/vk_fence_wake_probe_win.c): the vn FEEDBACK channel is
    0.61 ms while the WIRE-fence channel carries the 10-20 ms class.
    DOOM (instrumented, owner run): copy-lat 11.4 ms avg (65% in
    10-20 ms, 0% <1 ms), acquire-gate 6.9 ms of the 9.5 ms frame, game
    device 97% of fences 6-20 ms — CONFIRMED as THE fps ceiling.
    OWNER DECISION: QEMU fix out of scope → WORKAROUND: the ICD retire
    thread now observes exported-fence completion via the semaphore's
    GPU-written vn feedback slot (slots now allocated for exported
    timelines — self-signaled only on this stack; counter VA attached to
    the sync, detached before pool recycling; poll ladder yield/Sleep to
    a 50 ms budget; wire path = fallback + kill switch).
    `HELIOS_RETIRE_FEEDBACK` DEFAULT ON, =0 restores wire behavior
    (A/B-proven). vkcube: retire_lat 5.6-9.2 ms → 0.25-0.33 ms (100%
    <1 ms, fb fast=100%); copy-lat 13.6 ms → 0.8 ms; content advancing;
    kill-switch run bimodal again. retire_fb fast/fallback/wire counters
    in the perf summary. PENDING: owner Doom re-run (expect the acquire
    gate to collapse and fps to rise toward the ~200 sw-path bound);
    HELIOS_RING_NOTIFY_EAGER diag knob (mesa `4d0c7d21514`, default off,
    falsified as a lever); host sysctls to revert when debugging ends:
    kernel.yama.ptrace_scope=0 (→1), kernel.io_uring_disabled=2 (owner's
    call — QEMU fence behavior identical either way).
- **Capture path**: IddCx frame drop policy vs D3D12 copy queue saturation;
  KVMFR bandwidth; 10 bpc default.
- Candidates list from the NVIDIA fix era lives in ICD.md.

## Workstream 3 — D3D11 Conformance

**30th session (2026-07-07) — 3DMark bring-up: LUID gap FIXED, FL11 ceiling
root-caused.**

- **Vulkan/DXGI LUID identity gap — FIXED, DEPLOYED, VERIFIED (mesa
  `23b10bb6d80`, main `e0b462f`; ICD `vulkan_virtio-c2919595f95d`).** The venus
  ICD reported `VkPhysicalDeviceIDProperties::deviceLUIDValid=false` ("Phase 6
  concern" stub in `helios_init_renderer_info`), so no VkPhysicalDevice carried
  the guest WDDM adapter LUID that DXGI reports. UL/3DMark Steel Nomad (Vulkan)
  selects an adapter via DXGI then matches into Vulkan by `deviceLUID` → found
  nothing → "VkPhysicalDevice with device LUID X not found". Fix: plumb
  `helios->adapter_luid` (captured from D3DKMTEnumAdapters2 at open, == the DXGI
  AdapterLuid) into `info->id` (has_luid=true, node_mask=1, luid verbatim).
  Verified same-boot: vulkaninfo `deviceLUID dfb16300-00000000` == WDDM adapter
  `luid 00000000:0063b1df`, `deviceLUIDValid=true`. (LUIDs change per
  device-restart — the fix reads adapter_luid dynamically, tracks it. dxvk's
  own findAdapterByLuid / D3DKMTOpenAdapterFromLuid also benefit.) **Owner to
  re-test Steel Nomad.**
- **Fire Strike (D3D11 FL11_0) "no GPU" — the Helios adapter is FL10_0; being
  raised gate-by-gate. It is NOT a KMD/adapter ceiling (that theory falsified)
  — it's a sequence of UMD caps bugs.** The engine is genuinely FL11 (bridge
  creates the dxvk device at `D3D_FEATURE_LEVEL_11_0`; all FL11 DDIs wired).
  Everything is behind `HKLM\SOFTWARE\Helios!FeatureLevel11`, now an integer
  MODE (0=explicit FL10 fallback, 1=full FL11 and the absent-value default as
  of 2026-07-24, 2=diagnostic pipeline-only); knob=0 = exact FL10 baseline,
  dwm-safe. **THE tool that cracked it: the
  `Microsoft-Windows-DXGI` ETW provider prints d3d11.dll's exact rejection
  string** (the debug layer / DXGI InfoQueue / DBWIN / DxgKrnl-AzureTriage all
  gave 0 messages — the failure is device-less). Recipe: `logman start
  helios_dxgi -p Microsoft-Windows-DXGI 0xFFFFFFFFFFFFFFFF 0xff -o x.etl -ets` +
  `logman update helios_dxgi -p Microsoft-Windows-Direct3D11 ... -ets`, run the
  probe, `logman stop`, `tracerpt x.etl -o x.xml -of XML -y`, read `<Data
  Name="Message">`/`Code`. Gates cleared: (1) **"Driver returned invalid
  pipeline caps"** — 3DPIPELINESUPPORT is a BITMASK
  `(1<<Level)` OR'd, not the bare enum; we wrote 11_0=2 = bit1-only = invalid;
  fixed FL11=0x7, FL10=0x1 (the old 10_1=1 worked by luck = the 10_0 bit).
  (2) **"Driver doesn't support compute on FL11"** — SHADER caps now advertise
  0x2 (compute). (3) **"MSAA quality reported to be 0"** — FL11 requires every
  render-target format to support 4x MSAA and does NOT exempt 96-bit R32G32B32;
  CheckMultisampleQualityLevels now floors RT formats to >=1 at 1/2/4/8 +
  check_format_support advertises MULTISAMPLE_RENDERTARGET for RTs — PARTIAL:
  the runtime advances past formats 5-8 but still hits the MSAA error on a later
  format/count. **NEXT (owner directive): conform to the D3D11.3 functional
  spec** (https://microsoft.github.io/DirectX-Specs/d3d/archive/D3D11_3_FunctionalSpec.htm)
  — exact per-format/per-sample-count FL11 MSAA requirements. Committed main
  `ff14979` (WIP, un-deployed source; the all-count MSAA-log build was not
  installed). Probe: `tools/d3d11_fl_probe.cpp`, schtasks `helios_flprobe`,
  session 1 (session-0 win_exec fails all levels for a context reason, not FL).

**31st session (2026-07-07) — Fire Strike launch blocker moved past FL11:
legacy DXGI output mode-list fails on the IddCx logical output.**

- Owner rebooted after an IDD/client wedge; desktop composition is healthy again
  (`HeliosRenderAdapter=1`, both Display devices `OK`, IDD frames flowing with
  dirty-rect D3D11 fallback copies). The latest Fire Strike result
  (`3DMark-FireStrike-FAILED-20260707155504.3dmark-result`) exits both Demo and
  GT1 during `SINGLE_INIT_BEGIN` before workload rendering:
  `IDXGIOutput::GetDisplayModeList` returns `DXGI_ERROR_NOT_CURRENTLY_AVAILABLE`
  (`0x887a0022`), then 3DMark reports "Workload produced no results".
- Repro probe on the same boot: DXGI enumerates `\\.\DISPLAY6` under a fixed
  logical Helios adapter LUID `00000000:000078c5`; every
  `IDXGIOutput::GetDisplayModeList` / `GetDisplayModeList1` call returns
  `0x887a0022` for the tested formats. `D3DKMTOpenAdapterFromGdiDisplayName` on
  that same `\\.\DISPLAY6` opens the real Helios render adapter LUID
  `00000000:0038c127`, and `D3DKMTGetDisplayModeList` succeeds with 1064 modes.
  DXGI ETW around the probe records `IDXGIOutput_GetDisplayModeList` stop events
  with `m_Ret=2289696802` (`0x887a0022`) and the output object bound to
  `\\.\DISPLAY6`, but no richer rejection string.
- Falsified: RDP/session-context cause. The workload and probes run in active
  console session 1, not an RDS session. Also falsified: missing IDD modes in
  general (CCD/KMT mode lists exist), and UMD WDDM1.3 Present1/MPO callback
  table as the immediate launch blocker (current UMD logs show the DXGI 1.3 base
  table populated and D3D11 device creation succeeds in probes).
- Current read: this is the architectural split between the IddCx/IndirectKmd
  display adapter that owns the output and the real Helios render adapter that
  owns D3D/Venus. Fire Strike's legacy fullscreen init assumes
  `IDXGIOutput::GetDisplayModeList` works on the visible output. A proper fix is
  not a UMD present-path hack; it likely requires a real display/VidPN owner for
  the visible output (or equivalent OS-supported path that makes DXGI's output
  mode-list resolve on the render/display adapter pair). If reviving KMD display
  ownership, use the archived viogpu3d/VidPN research and treat it as a full
  display-miniport implementation, not as stubbed VidPN callbacks.

**32nd session (2026-07-07) — Fire Strike/FaceWorks missing surfaces: first
real D3D11 correctness bug fixed, owner visual validation pending.**

- Owner used Windows Settings -> Display -> Detect multiple displays; there was
  no visible display change, but Fire Strike moved past the legacy
  `IDXGIOutput::GetDisplayModeList` launch blocker and now reaches fullscreen
  rendering. Current symptom: many missing surfaces. Do not spend time on
  `DxgkDdiPresent`: this is a render-only adapter path, and the desktop already
  proves the presentation/capture leg is alive.
- FaceWorks is the faster repro. Its initial dxbc-spv assertion in
  `shd_instruction.cpp` came from D3D11 DDI tessellation patch-constant sysval
  names; `umd/bridge/dxvk_bridge.cpp` now remaps the DDI tess-factor sysvals to
  DXBC `SV_TessFactor` / `SV_InsideTessFactor` signatures. The sample then
  opened a black window in owner-visible testing. A scheduled-task run is not a
  valid visual proxy yet: it exits during DXUT validation before any present.
- Concrete bug found in `umd/src/forward.rs`: DDI Texture2D views were always
  translated to non-MS D3D11 view dimensions. For multisampled Texture2D
  resources this is wrong: RTV/DSV/SRV must use
  `TEXTURE2DMS`/`TEXTURE2DMSARRAY`, not `TEXTURE2D`/`TEXTURE2DARRAY`. This is a
  real correctness hole for Fire Strike-class MSAA workloads and can make view
  creation fail or bind the wrong resource interpretation.
- Fix deployed in UMD `helios_umd_ac10566f81de7294.dll` (SHA256
  `AC10566F81DE72944B472131DF1AE8CBAD719938DCC804C9A936FC14F6643B19`):
  `rtv_desc`, `dsv_desc`, and `srv_desc` now query the underlying
  `ID3D11Texture2D::GetDesc().SampleDesc.Count` and select the MSAA view
  dimensions/unions when `Count > 1`. Adapter hotplug only; no guest reboot.
  Active registry and live DWM/explorer modules point at the new ProgramData
  UMD, Helios device is `CM_PROB_NONE`.
- Evidence: new `tools/d3d11_msaa_view_probe.cpp` passes on Helios FL11_0. It
  creates 4x `R8G8B8A8_UNORM` RT/SRV and `D24_UNORM_S8_UINT` depth resources,
  creates explicit 2DMS RTV/SRV/DSV views, clears, resolves, stages, maps, and
  reads pixel `64,127,191,255`. UMD log for pid 10120 shows
  `MSAA q fmt=28 c=4 -> 1`, successful `create_rtv`, `create_srv`, and
  `create_dsv` on `dim=3` MSAA resources. **NEXT:** owner reruns FaceWorks and
  Fire Strike against this UMD; if surfaces are still missing, inspect fresh UMD
  logs for failed Create*View, ResolveSubresource, Copy/Discard/ClearView, and
  noop-DDI counter movement.

**33rd session (2026-07-07) — FaceWorks/Fire Strike "missing surfaces" reframed:
render is FINE, the problem is windowed compositing. Two earlier theories
FALSIFIED.**

- **FaceWorks is not a render/coherence bug.** `HELIOS_PRESENT_READBACK` shows the
  present source non-black at 1264×681 (multi-pass scene: 1060 draws to the
  backbuffer, 937 to a 184×161 SSS RT, DrawIndexed to 1024×1024).
  `tools/d3d11_shared_draw_probe.cpp` proves float + SINT-indexed+CB + textured/
  blend draws propagate cross-device via OpenSharedResource1. Owner: **Fire Strike
  renders fine fullscreen** — the render pipeline is healthy.
- **FALSIFIED — "two-memory split / KMD zeroes the adopted resid."** The UMD's
  `allocate_wddm_resource` "private mutated" log (`res_id 22301→0, blob→0x3b4000`)
  is the UMD reading the KMD's NEW `HeliosWddmOpenIdentity` writeback with the OLD
  `HeliosWddmAllocPrivate` layout — offset 0 is `venus_alloc_size` (0x3b4000 =
  3883008), and the fields it prints as res_id/kind are `reserved` words. The KMD
  adopt path (`create_allocation.rs adopt_blob_for_allocation`) preserves resid
  22301 and mints no redundant blob; the allocation IS backed by the DXVK venus
  image. Do not re-chase this.
- **FALSIFIED — alpha.** Owner confirmed `HELIOS_PRESENT_FORCE_OPAQUE` changes
  nothing; DWM composites the flip swapchain opaque.
- **FALSIFIED — the IDD.** Both the live D3D11 fallback (`SwapChainNewFrameD3D11`)
  and the D3D12 path capture the same single composed IddCx surface; the D3D12
  path is dead only because our UMD implements no D3D12. The IDD faithfully
  forwards whatever DWM composited (`CopyFromScreen`/paintcap show the same black
  client area).
- **The live root cause is the render-adapter / DXGI-output topology (→ priority
  #1 above).** DXGI enumerates TWO identically-named "Helios vGPU Render Adapter"
  entries (LUIDs …fba8, …7896) that both resolve to the same physical WDDM adapter
  (a stale-LUID residue from repeated device restarts) plus 2× WARP; every
  adapter's `EnumOutputs` returns `0x887a0022` intermittently (one run showed
  `outputs=1` on …7896). CCD `QueryDisplayConfig(ONLY_ACTIVE_PATHS)` pins the live
  Looking Glass output to …fba8 (`\\.\DISPLAY2`), while `QDC_ALL_PATHS` fails
  `ERROR_GEN_FAILURE` on a broken second path (ghost QEMU/DEFAULT monitors). Apps
  that need an `IDXGIOutput` get `NOT_CURRENTLY_AVAILABLE`, fabricate a mode, and
  drop the window off-screen; and DWM never imports the app's flip backbuffer.
  Probes: `tools/{dxgi_output_modes,ccd_adapter}_probe.cpp` (session 1). Memory:
  `phantom-adapter-luid-enumoutputs-33rd`, `faceworks-black-d3dflip-twomemory-split-33rd`.
- KMD 22.22.61: `create_allocation` now writes the `HeliosWddmOpenIdentity`
  trailer for adopted allocations too (live resid for cross-process openers).

Current state (surveyed 2026-07-05):

- UMD (`umd/`, Rust d3d10umddi frontend → cxx bridge → DXVK C++ engine):
  advertises `D3D11_1_DDI_SUPPORTED` (11.15.0) + `D3D11_0` (11.10.2), fills
  `D3D11_1DDI_DEVICEFUNCS`; `forward.rs` implements ~220 DDI functions;
  unfilled slots route to counted noop handlers (`ddi_noop_device/dxgi` —
  loud, not silent). Feature level 11_0 reported to the runtime.
- dxvk-helios: ~58 files / +3.8k lines diverged from upstream DXVK (venus
  import model, GDI staging, alias-image detile, persistent refresh,
  undersized-import refusal).
- Known DDI gaps (from bring-up sessions): ClearView logging blind (log
  budget), partial Discard handling fixed for the common case, Rotate
  implemented minimally; keyed mutex path exercised only by probes.

Plan:
1. Enumerate the noop-slot hit counters after a real workload day — every
   nonzero noop is a conformance gap with a caller.
2. Run dxvk-tests / d3d11-triangle / d3d9-on-11 samples inventory
   (`dx-samples-research-only/`), then 3DMark (installed on the VM).
3. DXGI format coverage audit (the format round-trip carrier landed at
   `bfb5121`; verify beyond BGRA8).
4. Map remaining 11.1 features (deferred contexts? threading modes? UAVs at
   FL11_0) against DXVK capabilities — most exist in the engine; the work is
   the DDI plumbing.

## Tooling (keep alive; this stage depends on it)

- **Registry knobs** (service key, active KMD reads): `DiagLevel`,
  `AllocCached`, `DisplayHalf`, `ScanoutDiag`,
  `DirectFlipCaps`, `CrossAdaptCaps`, `BarSegMode`, `BarSegFlags`,
  `BarSegBaseMB`.
  `DisplayHalf=1` enables the render+display adapter shape. `AllocCached=0`
  is the CpuVisible cached-allocation kill switch. `DirectFlipCaps` and `CrossAdaptCaps` are
  explicit cap-advertisement probes; leave off unless bisecting.
  `BarSegFlags`/`BarSegBaseMB` bisect BAR descriptor flags/base. `DiagLevel`
  enables the generic S-ring registry breadcrumbs.
  **`BarSegMode` now has exactly TWO legal values** (T4b/R904, KMD 22.22.187.0):
  `10` (default, absent = production: aperture id 1 + BAR id 2) and `0` (the
  recovery baseline: aperture id 1 + paging-RAM cpu-host id 2, no BAR). The
  historic Code-43 bisect arms `1`, `2`, `5` and `11` are DELETED, along with the
  `probe_only` BAR segment and its 16 MiB contiguous RAM block. Any other value
  is coerced to `10` and recorded in the new `BarMCo` counter carrying the stale
  number — so a VM left set from an old bisect now binds and says so instead of
  reporting a segment no allocation may use. Nothing reports segment id 3 any more.
- **ScanoutDiag modes** (service key, production default 0): `1` creates and
  binds a CPU-filled KMD blob once; `2`+ rebinds the diagnostic blob after OS
  scanout attempts; `3` tests shareable memory blob; `4`+ tests KMD-created
  Vulkan scanout images; `7` tests classic 2D `SET_SCANOUT`; `9`/`11` are
  CPU-filled cross-device image/blob variants; `11` and `16` use XR24; `12`
  and `13` test guest/host3d-guest blob memory; `14`/`15` test virgl HOST3D
  guest scanout helpers; `16` is the Linux/CachyOS-style LINEAR external DMA_BUF
  image path. Mode 16 is verified working after the tiling-constant fix, but the
  active value must remain absent/0 so it cannot overwrite the DWM primary.
- **Scanout counters** (service key fixed names): `Sc*` =
  `SetVidPnSourceAddress` scanout, `CSc*` = create-time scanout bind attempt,
  `PSc*` = Present/HWQ diagnostic-only scanout candidate, `Sdg*` = diagnostic
  scanout allocator/bind path, `Rf*` = periodic active-scanout refresh. Values
  persist across boots; trust movement plus same-boot QEMU traces.
- **SAMPLED counters, 22.22.180.0+** (R316): the `PB*` IDENTITY values written by
  `DxgkDdiPresent` — `PBcall PBflag PBcnt PBalst PBDma PBPatch PBpdsz PBkpsz`,
  the `PBs*`/`PBd*` surface identity sets, `PBstrk`/`PBdtrk`, and the flip arm's
  `PBsrc PBsw PBsh PBsDir PBIdOk PBFlip=1` — refresh on the 1st present and
  every 600th thereafter at `DiagLevel=0`, NOT per frame. **A `PB*` identity
  value can therefore be up to ~10 s stale; do not read one as live.** Set
  `DiagLevel=1` (+ `pnputil /restart-device`) to restore the per-call cadence.
  UNTHROTTLED, always current: `PBRet`, `PBCpy` (all arms), `PBFnc`, `PBSyWt`,
  `PBSyCp`, and `PBFlip`'s `0xE1`/`0xE2` failure arms.
- **RETIRED 22.22.180.0** (R903/x-dup-dead-20 — do not look for these; they are
  gone from the driver, and any value still in the service key is a stale
  leftover): the `GdiAccelMode` knob and the whole `Gd*` counter family —
  `GdiM`, `GdiE`, `GdiS`, `GdFa`, `GdFg`, `GdFs`, `GdFb`, `GdFm`, `GdFi`,
  `GdFr`, `GdTc`, `GdDs`, `GdCn`, `GdCr`, `GdCc`, `GdCg`, `GdBn`, `GdBr`,
  `GdBg`, `GdXn`, `GdXz`, `GdXr`. The KMD no longer advertises
  `SupportKernelModeCommandBuffer` in any configuration and no longer contains a
  GDI raster executor; GDI renders through win32k's CPU redirection path.
- **Counters** (service key): Ch* (CpuHostAperture),
  Pg* (paging engine; `PgEv` nonzero means unresolved virtual paging transfer),
  AE* (8-slot allocation create/open ring: resid, dimensions, ctx/open marker)
  — all failure counters must stay 0; S-ring breadcrumbs persist across boots
  and high indices go stale after short boots.
- **Direct-primary producer gate** (`HKLM\SOFTWARE\Helios`, not the service
  key): `PresentGateUs` is absent by default and the UMD uses 10000 µs. `0` is
  the ordering A/B disable. The wait is condition-variable-backed; inspect
  `present-gate:` in the current DWM UMD log for cost and timeouts.
- **Launcher/display path**: `tools/launch-helios-gtk.sh` supports
  `HELIOS_DISPLAY=egl-vnc` for `-display egl-headless` + VNC, intended as the
  reliable display-output inspection path, and `HELIOS_DISPLAY=sdl` is visually
  verified on native Wayland. It uses the `qemu-helios` submodule build, whose
  egl-headless/GTK/SDL OpenGL backends share exact OPTIMAL Vulkan readback when
  EGL cannot import a modifier-less native image. Interactive modes leave EGL
  vendor selection to the compositor while NVIDIA remains selected for
  Venus/readback Vulkan; globally forcing NVIDIA EGL breaks Wayland context
  creation on the development host. GTK is still blocked by its later GDK
  `eglMakeCurrent` failure.
  `HELIOS_QEMU_RENDER_GPU=nvidia` is the current owner preference; render-node
  defaults are tracked in the script. The old force-LINEAR LD_PRELOAD shims were
  experiments, not supported display paths. `HELIOS_QEMU_TRACE` can enable
  `virtio_gpu_cmd_set_scanout_blob`, `virtio_gpu_cmd_res_flush`,
  `virtio_gpu_cmd_res_create_blob`, and `virtio_gpu_cmd_ctx_submit`; the trace
  file `/tmp/helios-qemu-stderr.log` is ground truth for scanout shape.
- **ETW**: `logman create trace -p Microsoft-Windows-DxgKrnl 0xFFFFFFFFFFFFFFFF
  0xFF` → tracerpt → grep `AzureTriage` = dxgkrnl failure reasons in plain
  text. Found the segment rule in minutes.
- **AddAdapter iteration**: `pnputil /restart-device` re-runs AddAdapter with
  the loaded image — registry-knob experiments need no reboot.
- **T3 refusal instrument**: `ScForceReject` (service key, REG_DWORD, default 0
  and absent in production) forces ONE deferred-programming refusal exit so its
  counter can be proven to move: 1=BadAlloc 2=Extent 3=Layout 4=Format
  5=LinearAllocFailed 6=SetFailed 7=NoTarget 8=CopyFailed. Read at StartDevice,
  so `restart-device` between values — no reboot. Mirrored to `ScFrc`; a nonzero
  `ScFrc` means the `Sc*Err` values in that dump were PROVOKED, not observed.
  7 and 8 sit on the copy/fallback arm and are unreachable with a direct primary.
  T6 deletion candidate. ⚠ Knob names are capped at **14 chars**
  (`diag::MAX_CONFIG_NAME`) — now a build failure, previously a silent
  always-default.
- **T3 counters** (all must read 0; reset at StartDevice so movement is
  this-boot): `ScBadAlc ScBadExt ScBadLay ScBadFmt ScLinErr ScSetErr ScNoTgt
  ScCpyErr` (one per refusal class), `ScUnav` (HPD dropped a dirty bit),
  `ScRetry`/`ScGaveUp` (R506's bounded retry), `ScStale` (a completion tried to
  clear an interval that was not its own), `ScGateCx` (the DIRQL raise CAS
  exhausted its budget), `HpdStTo` (the HPD prologue fell back to its 500 ms
  bound instead of the real start edge).
- ⚠ **Kernel stack budget on the boot path**: `DxgkDdiStartDevice` +
  `VirtioGpu::init` are the binding chain — **17568 B of the 17936-B known-good
  ceiling as of 22.22.184.0** (8408 + 9160), i.e. 368 B of headroom in a 24 KB
  kernel stack. Overflow at boot = `0xc0000001`/Startup Repair with **no dump and
  no bugcheck event**, and it does NOT reproduce on a live `devcon` restart.
  **Run `tools/kmd-frame-sizes.ps1` on every image** — it reads the frames out of
  the built `.sys` + linker `.map` with `llvm-objdump` (no PDB, no debugger),
  handles sub-page frames, sums the declared call CHAINS rather than every symbol
  measured, and **exits 1 over the ceiling**. `-Symbols`/`-Chains` extend it.
- **Counter snapshots**: `tools/kmd-counter-snapshot.ps1 -Label <name>` dumps the
  whole service key to `Z:\tmp\kmd-counters-<name>.txt` and prints the
  transport/venus/scanout subset. Registry values PERSIST ACROSS BOOTS — take one
  before a workload and one after and diff the files; a single read proves nothing.
- **T4a counters** (22.22.184.0+; every one must read 0 or be absent on a healthy
  boot): `VnEncOvf` (venus command-stream overflow, absent), `VnRingFt`/`VnRingWd`
  (ring fatal latch / head-wait ms, absent), `VnRingSz` (undersized ring mapping,
  absent), `VnMtDown` (memory-type downgrade, absent), `CpNoDrn` (prepared-copy
  drain skipped because nothing was submitted), `PBTdErr` (partial Present-BLT
  teardown), `CtNotOurs` (sync token named another entry), `WtTbl`
  (`FENCE_WAIT_TABLE_FULL`, split out of `WtOut`), `AbnDrop` (fences discarded by
  a TDR/preempt/reset epoch), `ChSzMm`/`ChSzDl`/`ChSzPv` (aperture size-provenance
  cross-checks), `MapDup` (duplicate blob map refused at commit time),
  `PciCapOob` (PCI capability tail outside config space), `WnRcf` (window-reserve
  reconfiguration refused). CollectDbgInfo is **version 6 / `[u32; 38]`**, with
  `FENCE_WAIT_TABLE_FULL` at index 37; word 25 is still `FENCE_WAIT_TIMEOUTS`.
- **Guest probes** (schtasks, session 1; SSH lands in session 0):
  `helios_paintcap` (screenshot → `Z:\tmp\screen_copy.png`), `helios_repaint`,
  `helios_flasher`, `helios_dstate`, `helios_enum_windows`, `helios_regedit`.
  `FindWindow('Progman')` is broken on this box — EnumWindows only.
- **User-mode stack dumps**: `tools/take-minidump.ps1 -ProcessId <pid> -Path <dmp>`
  (P/Invoke MiniDumpWriteDump; the `rundll32 comsvcs.dll,MiniDump` trick writes
  TRUNCATED dumps on this box — do not use). Analyze on Linux:
  `~/.cargo/bin/minidump-stackwalk --symbols-path <breakpad-syms> <dmp>`;
  make syms with `~/.cargo/bin/dump_syms <pdb>` (dir layout
  `syms/<name>.pdb/<GUID+age>/<name>.sym`; fix the MODULE line name if the pdb
  was renamed). The deployed UMD build's PDB must GUID-match the dump's module
  (check with `llvm-pdbutil dump --summary`).
- **KMD build/deploy**: `win_build_kmd` (bumps the three version sites with a
  coherence check, then cargo-make package build) → `win_install_kmd`
  (install script + recommended, toggleable graceful guest reboot — the only
  reliable activation path). Manual fallback: `win_cargo` +
  `tools/install-helios-kmd.ps1` (ExecutionPolicy Bypass,
  `-AllowRebootRequired`); version bump = the single `HELIOS_KMD_VERSION` line in
  `kmd_render/driver-version.env` (build.rs renders the FILEVERSION numerics and
  the version strings from it; Cargo.make stampinf reads it via `env_files`);
  backups under
  `C:\ProgramData\HeliosDeployBackups`. New tools appear after the win MCP
  server restarts (new session).
- **dxvk staged-content probes** (`dxvk.heliosStagedProbes`, default OFF since
  `bdbbc2ea` — they were the ~1.5 s stall): full-surface raw+post-copy readback
  characterization at fixed refresh ticks for black-surface triage. Re-enable
  per process via `DXVK_CONFIG "dxvk.heliosStagedProbes = True"` (no rebuild).
- **Venus pipeline object trace** (`VN_HELIOS_PIPELINE_TRACE` env, per-process,
  default off): (ring, primary_tail seqno, object id) lines in the ICD diag log
  for pipeline-layout create/destroy, the vn_get_target_ring wait_all barrier,
  and graphics-pipeline creates — the defect-0b recurrence kit. The barrier
  skip/abandon lines (`BARRIER SKIPPED/ABANDONED in wait_all`) are ALWAYS on.
- **ICD sem-deadline strike log** (`helios_icd_diag.log`, always on): each strike
  line carries `sem=` (venus object id), `reason=` (vn_relax reason),
  `sig_queue=/family=/ring=` + `sig_value=/sig_age_ms=` (the most recent submitted
  signal op for that semaphore, recorded at submission prepare) and `pending_ms=`
  (how long a signal had been pending with zero movement — the quantity the
  deadline gates on). `sig_age_ms` near 0 on a strike = wait-before-signal false
  positive (should no longer happen post-f7a816f182f); large `pending_ms` = a
  genuinely stuck host channel.
- **Queue-submit phase timing**: `HELIOS_QUEUE_PERF=1` + `HELIOS_PERF_FILE`
  machine env (live in dwm since the 2026-07-06 reboot) — one aggregate line
  per 300 vkQueueSubmit2 calls (tls/wsi-flush/cache-flush/submit/fence-wait
  phase averages) to `C:\ProgramData\Helios\helios_queue_perf.log`.
- **Ring-fence probe**: `tools/vk_ring_fence_probe.cpp` → schtasks
  `helios_ringprobe` (wrapper `C:\Users\Rupansh\helios-probe\run_ring_probe.cmd`,
  `/rl LIMITED`; `helios_ringprobe_named` runs the NAMED-import mode against the
  dev ICD build via `icd_devbuild.json`). Proves/regression-tests the WS1 #4
  chain: rc=0 + "consumer wait tracked GPU completion". Build on the VM with
  the WinLibs g++ (`g++ -O2 -o ... Z:\tools\vk_ring_fence_probe.cpp -I <VulkanSDK>\Include
  C:\Windows\System32\vulkan-1.dll`) — no clang-cl on the box. **GOTCHA (cost a diagnosis detour): the Vulkan loader
  silently ignores `VK_DRIVER_FILES`/`VK_ICD_FILENAMES` in ELEVATED processes** —
  win_exec/SSH shells are High-IL (and `runas /trustlevel:0x20000` still reads as
  elevated), so an "env-override" probe actually tests the REGISTRY ICD. Run ICD
  A/B probes through a `/rl LIMITED` scheduled task.
- **QEMU fence tracing without restart**: QMP on `/tmp/helios-tpm/mon.sock` →
  `trace-event-set-state` for `virtio_gpu_fence_ctrl`/`virtio_gpu_fence_resp`
  (output → `/tmp/helios-qemu-stderr.log`; ctrl→resp gap per fence id = decode-
  vs GPU-completion retirement; disable after use — it logs 2 lines per fence).
  NOTE: `-d guest_errors` is already on, but virglrenderer's vkr_log/proxy_log
  are INFO-level = SILENT on the release build — absence of host log lines
  proves nothing below WARNING; a real host-side bisect needs a relaunch with
  `VIRGL_LOG_LEVEL=debug`.
