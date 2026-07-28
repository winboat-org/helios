# The Phase-1 refactor of `kmd_render` and `umd` — tranches T0 through T8

**FROZEN 2026-07-28. Completed and gated in full; nothing here is an open
action.** Lifted verbatim out of `ROADMAP.md`'s "Current priorities" when the
last tranche passed its gate, so the living stage document stops carrying 1570
lines of closed work. Read `ROADMAP.md` for the current state.

The review that produced the plan is `docs/archive/REFACTOR_REVIEW.md`; the two
prompts that started it are `docs/archive/REFACTOR_HANDOFF.md` and
`docs/archive/REFACTOR_PHASE2_PROMPT.md`.

Final state: **KMD 22.22.190.0 + UMD `DB343F02…`**, eleven tranches
(T0, T1a, T1b, T2, T3, T4a, R614, T4b, T5, T6, T7, T8), every one gated on
hardware. Sections 7m and 7n at the end are T8: the implementation and its
gate.

Two standing directives from this work, which is why they are ALSO summarised
in `ROADMAP.md` rather than only here:

* Never fold a `BUG` fix into a structure move; one recommendation per commit.
* Preserve the direct primary, completion ordering, loud-failure contracts,
  the registry ABI and every diagnostic name unless a reviewed change
  explicitly migrates them. Replace arbitrary `Sleep`/poll loops with event,
  interrupt, fence or condition-variable contracts — but do NOT remove a
  bounded safety timeout merely because it waits.

---

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

   **(3) R809 — the two behaviour changes. HALF DECIDED BY THE 7f GATE, half still unexercised.**
   ★ **DirectPrimary-wins: the gate read `SCANOUT_DIRECT_OVER_LINEAR` = 17 in one dwm session
   (2 and 1 in others), so this rule is REACHABLE and must not be dropped as dead.** Adopting
   it is a real behaviour change on the frozen direct-primary path and stays an owner-visible
   decision, but "delete it as unreachable" is now ruled out by evidence.
   **Down-resolution: `SCANOUT_DOWNRES_KEPT` = 0, but UNEXERCISED, not proven unreachable** —
   no resolution change could be driven on this box (see 7f). Do not conclude anything from
   that 0 until a mode change actually happens.
   `SCANOUT_ZERO_EXTENT` = 0 and `SCANOUT_PRIMARY_ZERO_PITCH` = 0, both as predicted.
   Original entry, for the reasoning:
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

7g. **T6 IN PROGRESS — "delete the proven-dead paths". Pre-deletion reachability evidence
   taken 2026-07-28 on KMD `22.22.187.0` / UMD `0daa30ca3ac146ec`, boot 2026-07-27 17:32.**
   Real scope is **16 items, not the review's 18**: R903 (gdi_blit) landed in T1b at
   22.22.180.0 and R904 (BarSegMode) landed in T4b at 22.22.187.0, both exactly as the
   review's own plan-ordering correction instructed.

   **Three owner sign-offs obtained before a line was touched**, plus one adjacent call:
   R901 *delete the lab*; R910 *delete all of it, the review's literal list*; R912
   *(a) retire the kwait subsystem*; `ScForceReject` *delete*.

   **(a) R901 — the ScanoutDiag lab is inert this boot.** `ScanoutDiag` ABSENT, so
   `diag_mode()` returns 0 and `maybe_run` returns at its `mode == 0` guard while
   `rebind_if_forced` returns at `diag_mode() < 2` — both *before* writing anything. Confirmed
   dynamically: all **40** `Sdg*`/`S2d*` values are byte-identical before and after a mixed
   session (two `helios_dcomp_probe` runs, `helios_vkcube`, `helios_notepad_max`,
   `helios_paintcap`). ⚠ **`SdgReb=235`, `SdgRv=11`, `SdgRRid=4`, `SdgRPc=5120`, `SdgRSet=1`,
   `SdgRFlu=1`, `SdgMc=3` are STALE from an old `ScanoutDiag>=2` boot** — `maybe_run` zeroes
   seventeen `Sdg*` names plus the eight `SdgL*` each StartDevice, but **not** the `SdgR*`
   rebind family, so those never got cleared. Presence is not evidence; the diff is.
   ⚠ The stronger form of this test — *delete the values, then confirm they do not reappear* —
   **could not be run**: the registry-write was refused by a permission gate. The
   before/after diff over a real workload plus the static argument is what stands in.
   **Production `Sdg*` names that must survive R901**, confirmed by grepping the writers
   rather than by the review's list: `SdgLStg SdgLReq SdgLBit SdgLTyc SdgLImg SdgLMem SdgLPch
   SdgLOff` (the production LINEAR ladder, `venus.rs` `create_linear_scanout_image` /
   `allocate_linear_scanout_image_blob`), `SdgMt SdgMf SdgBFl` (written by BOTH the diag and
   the production allocator) and `SdgDevR SdgDevX` (device create). Live values this boot:
   `SdgLStg=16 SdgLReq=7910400 SdgLBit=15 SdgLTyc=5 SdgLPch=7680 SdgDevX=1 SdgDevR=0`.

   **(b) R907 — `PHQcall` is ABSENT from the service key entirely**, i.e.
   `dxgkddi_present_to_hw_queue` has never been called in the key's lifetime, across every
   desktop session and Fire Strike run since it was created. No `HwSched`/`HwQueueSupported`
   capability is advertised anywhere in the driver. Evidence gate met.

   **(c) R910 — `scanout_copy_count == 0`, but the import IS taken.** The
   `DWM desktop->LINEAR scanout copy #N` line is **absent** from dwm's whole session, which is
   the x-dup-dead-14 precondition. `open_kmd_scanout_target … res=6 1896x1030 pitch=7680
   fmt=87 gen=3 opens=4` **is** present — so `ensure_kmd_scanout_target` runs and imports the
   KMD target on every DWM boot while the copy never fires. That is exactly what
   `forward.rs`'s own comment predicts ("merely opening the LINEAR target does not qualify —
   that happens on every DWM boot while this count stays 0"). ⚠ Honest limit: no non-direct
   primary could be forced (no mode change possible on this box, see 7f), so this is
   "the copy never fires on an ordinary desktop", not "the copy is unreachable".
   ⚠ **The review's item list is wrong here and following it literally would have deleted the
   direct-primary recorder without noticing:** `remember_scanout_target` writes
   `ScanoutKind::DirectPrimary`, it is not a LINEAR function. Deleting the full list also
   deletes `SCANOUT_DIRECT_OVER_LINEAR` / `SCANOUT_DOWNRES_KEPT` / `SCANOUT_ZERO_EXTENT` —
   the instrument T5 built for R809. The owner chose the full deletion knowing that, on the
   ground that with no LINEAR import left there is no competition for the rule to govern:
   **R809's deferred DirectPrimary-wins rule closes as vacuous, not as adopted.**

   **(d) R912 — the measurement gate is SATISFIED.** `helios_vkcube`, the Vulkan vehicle
   workload the review names: `vehicle present #1536: imports_failed=0 copies_failed=0
   geom_mismatch=0 overwrites=0 **kwait_armed=0** kwait_arm_fails=0 kwait_queue_fails=0`, and
   `get_present_result: none pending (**x1537**)`. Armed is **0** over 1536 vehicle presents;
   misses track presents one-for-one (the two counters log on different cadences — presents at
   `(n+1) % 512`, misses at `n % 512` — so 1537 vs 1536 is the same sample point, not a
   discrepancy). Every ICD call falls back to the serial `wait_last_present`.
   ⚠ **`helios_umd_get_present_result` must keep existing under outcome (a)**:
   `icd/mesa/src/vulkan/wsi/wsi_common_win32.cpp:880-886` resolves all three UMD exports by
   name and fails the whole vehicle path with `E_NOINTERFACE` (incrementing
   `helios_vehicle_export_miss`) if any is missing. It stays, returning -1.

   **Rendering evidence for the pre-deletion state:** `helios_paintcap` at 01:13:30 — full
   composited desktop, wallpaper, taskbar, a maximised Notepad, a PowerShell console and the
   clock reading 01:13 28-07-2026. DComp probe `PROBE PASS`, 1357 frames / 25 s.

7h. **T6 IMPLEMENTED — all 16 items, 21 commits, KMD `22.22.188.0` + UMD
   `355b4366b1666104`. UMD HALF DEPLOYED AND VERIFIED; KMD HALF AWAITS A REBOOT.**
   A KMD image only loads at boot, so `22.22.188.0` is built, signed and packaged
   but NOT yet the running driver — every KMD number below is from
   `22.22.187.0`. The UMD half needed only `pnputil /restart-device` and is live.

   **Deployed and measured (UMD `355b4366b1666104`, five `restart-device` cycles):**
   ```
                        T5 BEFORE            T6 AFTER
   ownership soak       device 300 = 1946    1947      (5.99 handles/device both)
     300 dev/10000 res  resource flat 1977   flat 1975 (all ten samples)
                        final 1952 / +0 mod  1953 / +0
                        failures 0 / 0       failures 0 / 0
   Fire Strike Graphics 20212                20214     (GT1 141.1/140.89, GT2 63.8/63.86)
   D3D11 suites         TOTAL failures=0     TOTAL failures=0, xproc_read_rc=0
   present-gate (dwm)   n=3072 avg 2018 us   n=8704 avg 1852 us, 0.8% timeouts
   ICD faults           4 procs, 0xc0000005  SAME 4 procs, only 0xc0000005
   ```
   Rendering evidence: `helios_paintcap` after the deploy + restart — full
   composited desktop, taskbar, Notepad, PowerShell, clock reading the capture
   minute.

   ⚠ **DComp cadence looked like an 11% regression and IS NOT. A/B'd rather than
   argued.** Five T6 samples on a settled idle box read 1149/1213/1152/1184/1161
   against T5's recorded 1316/1311. Redeploying the backed-up T5 UMD
   (`helios-umd-backup-t5.dll`), same box, same procedure, minutes apart, gave
   **1318 / 1279 / 1144 / 1198**. T5's own spread (1144–1318, 15%) fully contains
   the T6 range, and T5's low sample is below every T6 sample. **The probe's
   sample-to-sample variance on this box is wider than any tranche's effect** —
   which is what `kmd-gate-surface.ps1`'s own doc has said all along, and what
   the T0 gate flagged as an open WS2 question. Do not read a single cadence pair
   as a tranche result again; A/B or say nothing.

   **★ R911's counters earned their keep on first read, and corrected the review.**
   `srv_raw_hazard` is **3,892,049** in the 3DMark process (795k, 574k, 120k in
   others): `pfnResourceReadAfterWriteHazard` for SRVs is a HOT-PATH DDI whose
   body was empty and uncounted. `discard_partial` = 1 in several dwm/app
   processes. But **`gs_so_declaration_dropped` and `tess_sig_fallback` both read
   0 even under a full Fire Strike run** — the review predicted those two would
   move and name a WS3 conformance gap. They do not on this workload; the gap is
   real in the code but Fire Strike does not exercise it. All other counters 0.
   Cost of the added `fetch_add` on a 3.9M-call path: not measurable — Fire
   Strike Graphics moved 20212 → 20214.

   **Built, not yet running (KMD `22.22.188.0`):**
   - `tools/kmd-frame-sizes.ps1` PASSES: deepest boot chain **17584 B, headroom
     352** — byte-for-byte the pre-tranche figure. ⚠ It did NOT pass first time:
     R901's re-homed eight-name `SdgL*` zeroing array grew
     `dxgkddi_start_device` 8424 → 8456 and took headroom to 320. The tool still
     exited 0 (under the ceiling), so it would have shipped. Moved behind
     `#[inline(never)]` (`zero_linear_scanout_breadcrumbs`), the same annotation
     `bring_up_venus` carries for the same reason. **The prediction that R901
     would IMPROVE this number by deleting a StartDevice callee was wrong.**
   - KMD warnings 8 → **3**, and R906/2 removed the crate-wide
     `#[allow(dead_code)] mod virtio` that had been hiding them. That surfaced 14
     items; ten deleted (eight orphaned by this tranche's own deletions), four
     kept with narrow per-item allows carrying their reason. ⚠ `IMAGE_TILING_LINEAR`
     — the 39th session's root cause — sits among the DRM-modifier constants that
     went and was checked BY HAND, not by the compiler; it is live and stays.
   - UMD warnings 16 → 14 (both remaining are pre-existing).

   **Corrections to REFACTOR_REVIEW's T6 section, all verified against the tree:**
   R903/R904 already landed (T1b/T4b), so the real scope was 16 items, not 18.
   R906 sub-commit 1 was already done by T4b's `WddmSurface`. R911 commit 3 was
   already done by T5's R827 (`ShaderStage`). R905 says to delete
   `gpummu::MEMORY_SEGMENT_ID` and `BAR_SEGMENT_ID` — the first is LIVE at
   `start_device.rs:123` and the second no longer lives in `gpummu.rs`; deleting
   either would have been a build break. R915 says `ResourceContext::_marker` is
   never read — it is read twice, as the "is this a handle we minted" test. R910's
   item list names `remember_scanout_target`, which is the DirectPrimary recorder,
   not a LINEAR function. R901's commits (3) and (4) had to be SWAPPED, because
   `venus::allocate_scanout_image_blob` is the only caller of
   `ctrl::resource_assign_uuid`.

7i. **T6 GATE RUN AND PASSED (2026-07-28), KMD `22.22.188.0` + UMD
   `355b4366b1666104`, cold boot 02:58:36.** Installed with `win_install_kmd`
   (package UMD hash verified `355B4366…06245F`), one graceful reboot, then two
   `pnputil /restart-device` cycles. Backup at
   `C:\ProgramData\HeliosDeployBackups\20260728-025815`; the T5 and T4b UMDs are
   still at `helios-umd-backup-t5.dll` / `-t4b.dll`.

   **★ NO BOOT LOOP.** The tranche touched `DxgkDdiStartDevice` and the frame
   budget, so this was the real risk: `CM_PROB_NONE`, `DriverVersion 22.22.188.0`,
   23 s after boot. Deepest boot chain **17584 B / 352 B headroom**, unchanged.

   | Gate line | Result |
   |---|---|
   | Cold boot to `CM_PROB_NONE` | **yes**, 22.22.188.0 |
   | DxgKrnl all-keywords ETW, `AzureTriage` | **NONE** (armed across a restart-device + a dcomp run) |
   | Visible desktop, cold boot | `helios_paintcap` 02:59:58 — full desktop, wallpaper, all icons, taskbar, clock matching |
   | Visible desktop, after `restart-device` | `helios_paintcap` 03:01:43 — same |
   | `ScanoutDiag` absent, `VpSA`, `ScSet` | absent; `VpSA=1`, `ScSet=1`, `ScFlu=3`, `ScPch=7680`, `DspMd=124257286` |
   | KMD must-be-zero surface | **all clear** (`kmd-gate-surface.ps1` exit 0) |
   | R901 ext ladder | `SdgDevX=1` (export trio), `SdgDevR=0` — **tier numbering preserved exactly**, a DisplayHalf boot still reads 1 |
   | R901 production `SdgL*` ladder | `SdgLStg=0x10`, `SdgLReq=7910400`, `SdgLBit=15`, `SdgLTyc=5`, `SdgLPch=7680` — every value identical to the pre-tranche reading |
   | R901 lab names | `SdgM=0`; `SdgReb`/`SdgRv`/`SdgRRid`/`SdgRSet`/`SdgRFlu` **byte-identical to the pre-tranche reading** — nothing writes them now. `S2d*` absent |
   | R907 | `PHQcall` **absent**, and the new `HwQRef` **absent** — `CreateHwQueue` was never called either |
   | R902 | `RfUnb` **absent** across a cold boot AND a `restart-device` |
   | UMD gate surface | `umd-gate-surface.ps1` **CLEAN**, exit 0 |
   | D3D11 suites | `TOTAL failures=0`, `xproc_read_rc=0` |
   | Fire Strike, full T6 stack | Graphics **20003** (GT1 140.72, GT2 62.93, Physics 35587, Combined 5529) |
   | Ownership soak 300/10000 | device 300 = **1947**, **5.99 handles/device**, resource phase **flat at 1975** all ten samples, final 1953, modules **+0**, failures **0/0**, dwm handles **+0** |
   | DComp cadence | 1247, `PROBE PASS` |
   | Same-boot host evidence | `vulkan-readback: DMA-BUF import tiling=OPTIMAL … OPTIMAL DMA-BUF ready 1896x1030` — the exact OPTIMAL DWM primary |
   | dxgkrnl / TDR / bugcheck (System log) | **none** |

   Ownership is identical across all three stacks measured today — T5 (1946 /
   5.99 / flat 1977), T6 UMD-only (1947 / 5.99 / flat 1975), T6 full (1947 /
   5.99 / flat 1975). The printed `OWNERSHIP SOAK FAIL` is the pre-existing
   6-handles-per-device teardown leak (7d(b)) failing the literal "flat" test,
   identically on all three.

   ⚠ **A Fire Strike run that reported a score of 0 was nearly accepted.** The
   first attempt on this image finished in 61 s (a real run is ~6.3 min) because
   two D3D11 probe suites were launched concurrently; its `Result.xml` carried
   `firestrikegraphicsscorep = 0`. **Check the run DURATION, and open the XML —
   3DMark writes a result file either way.** The clean re-run gave 20003.

   ⚠ **`RfUnb` staying absent across `restart-device` is NOT proof the refusal
   works** — it is the same distinction T1a found for `StRst`: `restart-device`
   re-runs AddDevice with a FRESH zeroed context, so `active_scanout_resource`
   is 0 too and the `host_bound != active` mismatch never arises. Reaching it
   needs a PnP stop/start on the SAME context. The R902 refusal is therefore
   correct-by-construction and **unexercised**, not verified.

   ⚠ **WS1 defect 0z reproduced, PROMPTED, and slightly wider than recorded.**
   Eight `0xc0000005` faults this boot, at 03:01:11 and 03:02:28 — both instants
   coinciding exactly with the two `restart-device` cycles, i.e. provoked, not
   unprompted, and the historical process set (dwm, Explorer,
   StartMenuExperienceHost, SearchHost, ApplicationFrameHost) with
   `vulkan_virtio-*.dll` as the faulting module. **One entry differs from the
   recorded signature:** `ApplicationFrameHost.exe` at 03:02:28 faulted in
   `ucrtbase.dll`, not the ICD — same exception code, same burst, but 0z has
   only ever been recorded as ICD-module faults. Worth a line in the WS1 item.
   dwm has been stable since 03:02:28. Zero dxgkrnl/TDR/bugcheck events.

   ⚠ **NOT performed, and owed from earlier tranches:** **rapid cursor motion /
   no trails** (needs an interactive mouse; NINE tranches overdue and still the
   only gate line no instrument covers), **suspend/resume** (not testable,
   `disable_s3=1`/`disable_s4=1`), a **resolution change**
   (`ChangeDisplaySettingsEx` is not an available mechanism on this box, 7f), and
   the **one-hour unattended session** — the box was under continuous test
   instead.

7j. **T7 IMPLEMENTED — all 16 items, 22 commits, KMD `22.22.189.0` + UMD
   `3b704b27b42a3ef1`. UMD HALF DEPLOYED AND VERIFIED; KMD HALF AWAITS A
   REBOOT.** T7 is "de-duplicate and consolidate the surviving code", and its
   whole claim is **byte-level output identity**: no wire bytes, no descriptor
   bytes, no DDI slot values, no diag/breadcrumb values, no counter movements.
   "It still works" is not the gate here; "it emits the same bytes" is.

   Scope is **16 items, not the review's fourteen-plus-two**: R1015 folded into
   R1006 as the review directs, and every other item survived — but six of them
   lost a third to a half of their subject to T1b/T3/T4a/T4b/T5/T6, and three
   had already been fixed outright.

   **Deployed and measured (UMD `3b704b27b42a3ef1`, one restart-device):**
   - DWM composites live on the T7 UMD: `helios_paintcap` at 04:51:59 and
     04:53:47 show real window content, correct chrome and fonts, an active
     title bar and a drawn caret — i.e. focus changes and cursor blink are
     rendering, not a frozen last frame.
   - **★ R1014(1)'s decisive test PASSED.** `tools/shader-dxbc-ab.ps1` (new)
     pairs each `wrapped` DXBC dump with the `raw` it was built from and emits
     a build-independent `sha256(raw) -> sha256(wrapped)` map. T6 UMD vs T7 UMD,
     twice: **five common raw shaders, ALL BIT-IDENTICAL including the MD5
     stamp**, spanning both the 2-chunk signature wrapper (vs/ps) and the
     3-chunk tessellation wrapper (hs/ds). Three T6 shaders did not reappear —
     the extra suite created fewer shaders on the later runs — so that is
     missing coverage, not a difference.

   **Built, not yet running (KMD `22.22.189.0`):**
   - **★ FRAME SIZES BYTE-FOR-BYTE IDENTICAL TO T6** — every function and both
     chains: `dxgkddi_start_device` 8424, `VirtioGpu::init` 9160,
     `bring_up_venus` 1240, `allocate_host_visible_blob` 1376,
     `VenusRing::bring_up` 40, `into_instance` 704, `into_device` 888,
     `create_device_with_ext_ladder` 792; deepest chain **17584 / headroom
     352**, second chain 12720. This tranche rewrote every venus reply decode,
     all eight image/memory encoders, all 27 barriers and the segment writers —
     all on or beside the boot chain — and moved it by zero bytes. (T6 shipped a
     32-byte regression here that the tool passed, because it exits 1 only OVER
     the ceiling.)
   - KMD warnings **3**, the identical set. UMD warnings **14**, the identical
     set (both rustc ones pre-existing).
   - `kmd_logic` host tests **37 → 46**, all green: nine new R1002 golden-byte
     tests. `protocol` 8, green.

   **★ Two new host-side oracles, both of which run on LINUX in seconds:**
   - **R1002 golden bytes** live in `kmd_logic` (`cargo test`), and their
     literals were produced by compiling the **PRE-CHANGE** inline encoder
     sequences and printing their output — so each test is an equivalence proof
     against the old code, not a restatement of the new. **Fault-injected to
     prove they bite:** moving the export struct's `handleTypes` ahead of the
     nested dedicated fields (which still compiles and still type-checks) fails
     `export_dedicated_memory_allocate_keeps_its_nesting_order` immediately.
   - **R1010's `0..=200` format equivalence test** runs via
     `tools/format-table-check.rs` — `rustc --test --edition 2021`, no VM. The
     116 table rows were GENERATED by compiling the original eight `match`
     bodies, not transcribed.

   **Nine scope corrections to REFACTOR_REVIEW's T7 section, all tree-verified.**
   Three items had already been done by earlier tranches and are recorded as
   such rather than re-done:
   - **R1011's only claimed static gain is already banked** — T5/R827 replaced
     the `stage: &str` dispatch and its fail-open `_ => {}` with an exhaustive
     `ShaderStage`. The review then says to dissolve those two `_common`
     functions into the macro; **deliberately not done**, because deleting
     `ShaderStage` to replace a typed exhaustive dispatch with six macro
     expansions would UNDO T5's guarantee for no gain. R1011 landed as ONE
     commit of boilerplate removal, not the review's six.
   - **R1009's named drift is already fixed** — T5/R812 installs
     `pfnCheckDeferredContextHandleSizes` with its real signature in all three
     tables and it is in no `calc!` list. The three pre-change lists were
     extracted and compared programmatically: 18 entries each, ALL THREE
     IDENTICAL.
   - **R1006's `&AdapterKnobs` threading is already landed** (T4b/R703).
   Six lost subject matter to T6:
   - **R1002**: three image encoders, not four (`create_scanout_image` went with
     R901), and `ImagePNext::ExternalMemoryWithModifierList` would have ZERO
     users — the modifier constants went with R906. The half that stands is
     real: `IMAGE_TILING_OPTIMAL` did not exist and two encoders wrote a bare
     `w.u32(0)` for it, the 39th session's defect shape inverted.
   - **R1008**: four knob readers and TWO policies, not six and four. The
     interesting one, absent-means-ON, was `VehicleKernelFlipWait`'s and went
     with R912 — as did the item's stated Risk and its `EXT_KWAIT_ARMED`
     validation line.
   - **R1009**: three interface levels, not four (R918 deleted the WDDM2.1
     chain), so there is no `upgrade_wddm2_1`.
   - **R1013**: FOUR of its six named divergences are already gone (R910/R912).
     Of the three the handoff flagged unverified, all three SURVIVE — and the
     `PRESENT1_LOG_COUNT` double-increment is worse than recorded:
     `dxgi_check_present_duration_support` shares the same throttle as a THIRD
     consumer, so its `n < 64` windows cover fewer than 32 presents. Left alone;
     changing it is a log-cadence change.
   - **R1007**: its BUG half is gone exactly as it anticipated (T1b deleted
     `gdi_blit.rs`), and `display.rs`'s stride chain was already unified by
     T3/R507. Two consumers and one producer, not six sites. `PitchDiverge` and
     the `GdiAccelMode=1` validation arm are obsolete and were not added.
   - **R1006**: its "Invalid sequence permitted" paragraph and its stated reason
     the v3/legacy writers must not be deleted are built entirely on flipping
     `RAISE_WDDM_3_2_GPUMMU = false`. **That lever does not exist** — T4b/R701
     replaced it with `WddmSurface`. The conclusion holds; the reason is
     re-derived in-file.
   And three counts were simply wrong:
   - **R1001**: 28 decode sites, not 29, spelled `self.reply_map()` ×22,
     `&self.ring.reply_map` ×3, `&self.reply_map` ×3, and **zero**
     `client.reply_map`. There are also TWO `ring_command_reply` definitions
     where the review assumes one.
   - **R1003**: 23 image + 4 buffer barrier sites, which is what the review
     predicts after T6 — confirmed, and the two at the old `:4208`/`:4224` are
     gone and were NOT resurrected.
   - **R1016**: there is a THIRD flattener, `flatten_tess_io_signatures_11_1`,
     and it shares the 3-word tess header.

   **Three deliberate deviations from the review's designs, each stated in its
   commit** — in every case the review's own shape carried a risk this tranche
   is not allowed to take:
   1. **R1003(b)**: the review's `TransferPlan` enum merges the five recorders'
      bodies and names its own risk as "where a barrier could be dropped for one
      variant". Extracting only the LIFECYCLE (`record_reusable`) removes the
      same duplication without merging any body, so each barrier sequence stays
      verbatim at its site and still diffs against the pre-change source.
   2. **R1004**: the review's `submit_venus_async_ring1(.., notify: Option<..>)`
      would put an `Option<notify>` parameter back one layer above the place T3
      deliberately removed the `(ring, notify)` pairing from the type space —
      the mismatch that once left `vidpn_programming` latched at 1 and
      suppressed every further CRTC_VSYNC for a whole boot. Only the prologue
      and outcome mapping were extracted.
   3. **R1011**: see above — keeping `ShaderStage`.

   **Two behaviour additions, both deliberate and both named:**
   - `UMD knob: <name>=<value>`, logged once per process next to `UMD module:`.
     It is R1008's own validation instrument (the defaults moved from four
     hand-written tail expressions into constructor arguments, and this line is
     what proves the resolved values did not move with them) and it forces the
     four `OnceLock`s at adapter-open rather than at first use — same value
     either way, since the documented A/B is "write the value, then start a new
     process".
   - Two new `DdiRefusals` fields, taking R911's nine to **eleven**:
     `alloc_meta_format_unknown` (the silent `D3DDDIFMT_UNKNOWN` stamped into
     the KMD allocation meta) and `readback_stride_unsafe`. The `DDI refusals:`
     line gains two fields and `tools/umd-gate-surface.ps1` is re-anchored to
     eleven names.

   **One BUG fix, committed separately and labelled as one** (R1010 commit 3):
   `maybe_log_present_readback` used `dxgi_bytes_per_pixel` — a pitch-PADDING
   estimate whose 4-byte default covers the genuinely 16-bpp B5G6R5 /
   B5G5R5A1 / B4G4R4A4 formats — as a byte-addressing stride, so
   `(Width - 1) * 4` ran past the row and, on the last row, past the end of the
   mapping. Now bounded against the RowPitch the runtime actually mapped.
   Env-gated and capped at 8 invocations, and the desktop never presents such a
   primary: an out-of-bounds read that has never been observed to fire.

   **New tools:** `tools/kmd-check.ps1` (the KMD's own rustc diagnostics —
   `win_build_kmd` builds the UMD too and its ~115 clang warnings pushed the KMD
   warning count, a gate line since T4a, off the top of the captured output),
   `tools/shader-dxbc-ab.ps1`, `tools/format-table-check.rs`.

   **Owed, recorded rather than guessed:** whether the production surface ever
   takes the QUERYSEGMENT3/legacy paths (R1015). The `0x0912_*`/`0x0913_*`
   records that would show it are `diag::record`, DiagLevel-gated, and DiagLevel
   is cached at driver load, so answering it needs a DiagLevel=1 boot.


7k. **T7 GATE RUN — KMD HALF PASSED, UMD HALF FAILED. `helios_umd.dll`
   crash-loops dwm and LogonUI at cold boot; the QEMU window is BLACK.**
   Cold boot 2026-07-28 12:22:12 on KMD `22.22.189.0`. **BISECTED, and the
   answer is unambiguous: the defect is in the T7 UMD half, not the KMD half.**

   | Stack | Result |
   |---|---|
   | T7 KMD `22.22.189.0` + T7 UMD `3b704b27` | dwm + LogonUI crash-loop, **black screen** |
   | T7 KMD `22.22.189.0` + T6 UMD `355b4366` | **renders fine** (owner-confirmed) |
   | T6 KMD `22.22.188.0` + T7 UMD `3b704b27` | rendered fine for ~5 min, but WARM ONLY (restart-device, already-logged-in session — LogonUI was never re-exercised) |

   The box is currently left on **T7 KMD + T6 UMD, working.**

   **★ THE FAULT IS A SINGLE DETERMINISTIC SITE.** Every crash, in both
   processes, reports the SAME `Fault offset: 0x8068c`, exception `0xc0000005`.
   Resolved against `umd/target/release/helios_umd.pdb`
   (ImageBase 0x180000000, so VA 0x18008068c):

   ```
   llvm-symbolizer --obj=helios_umd.dll --demangle --functions=linkage 0x18008068c
     std::_Atomic_integral<unsigned int,4>::operator++       atomic:1469
     dxvk::ComObject<ID3D11VertexShader>::AddRefPrivate      com_object.h:59
     dxvk::ComRef_<D3D11Shader<ID3D11VertexShader,..>>::incRef  com_pointer.h:37
     dxvk::Com<D3D11Shader<ID3D11VertexShader,..>>::operator=   com_pointer.h:76
     dxvk::D3D11CommonContext<D3D11ImmediateContext>::VSSetShader
                                                     d3d11_context.cpp:1397
   ```

   i.e. **`VSSetShader` AddRef'ing a bad `ID3D11VertexShader` pointer.** The
   two T7 items that touch that exact path are **R1011** (the
   `stage_set_shader!` macro — `vs_set_shader` is the one member with the
   `ia.bound_vs_com` asymmetry) and **R1016** (`SigWords`, whose only consumer
   is `create_vs_input_variant`, reached through `bound_vs_com`). **R1009** is
   the third candidate: a wrong device-funcs slot would hand the runtime a
   shader handle that never held a real COM pointer.

   **KMD half of the gate, all PASSED and worth keeping:**
   - Cold boot to `CM_PROB_NONE` **first try, no boot loop** — the real risk,
     since this tranche touched `DxgkDdiStartDevice`.
   - `kmd-gate-surface.ps1` **CLEAN, exit 0**; all must-be-zero clear;
     `ScanoutDiag` absent; `VpSA=1 ScSet=1 ScPch=7680 DspMd=124257286`,
     identical to 7i.
   - Every T7-critical breadcrumb **identical to the T6 gate**: `SdgDevX=1`
     `SdgDevR=0` (R1001 rewrote that decode), `SdgLStg=16 SdgLReq=7910400
     SdgLBit=15 SdgLTyc=5 SdgLPch=7680` (R1002 rewrote that encoder),
     `BarF=28 BarB=0` (R1006's knob default), `SdgM=0`, the `SdgR*` lab family
     byte-identical, `PHQcall`/`HwQRef`/`RfUnb` absent, `VnEncOvf` absent, and
     `CpImgVr`/`CpMemVr`/`PBBufVr` **absent** — the three raw-VkResult
     breadcrumbs R1001 re-plumbed never fired.
   - Frame sizes byte-for-byte identical to T6 (see 7j).

   ⚠ **A near-miss worth recording: the host log's `vulkan-readback: OPTIMAL
   DMA-BUF shape mismatch required=8773632 fd_size=7913472` is PRE-EXISTING and
   is NOT this regression.** It first appears 2026-07-26T21:41:56 and occurs 94
   times across the log, alongside 284 `OPTIMAL DMA-BUF ready 1896x1030`
   successes. It looked exactly like a T7/R1002 encoder regression and is not
   one. Check `grep -n` for the FIRST occurrence before blaming a tranche for
   any host-side line.

   ⚠ **The UMD half was "verified" WARM and that was not enough.** The T7 UMD
   was deployed with `pnputil /restart-device` into an already-logged-in session
   at 04:50 and composited correctly for minutes; the DXBC-container A/B against
   the backed-up T6 UMD came back bit-identical. None of that exercised
   LogonUI or a cold-boot device create. **A UMD-only tranche still needs a cold
   boot before it can be called verified** — restart-device is not a substitute,
   for the same reason `StRst`/`RfUnb` cannot be provoked by it (fresh zeroed
   context, T1a and 7i). **7l corrects this further: that warm window was not a
   pass at all, it was a crash-loop behind a stale primary.**


7l. **T7 UMD CRASH FIXED and the whole T7 gate PASSED.** One commit, `ead692e`,
   `umd/bridge/dxvk_bridge.cpp` only. KMD unchanged at **22.22.189.0**; UMD
   **`ba6adde35da4426e`**. Cold boot 2026-07-28 12:56:43.

   **ROOT CAUSE — `bridge_guard` truncated every returned pointer to 32 bits.**
   R1014 commit 4 (`919f28a`) folded nine hand-written catch triples into

   ```cpp
   template <typename R, typename Fn>
   R bridge_guard(const char* what, R on_error, Fn&& fn) noexcept;
   ```

   `R` is deduced from `on_error` **alone** — the guarded body's return type is
   not a deduction context. Four call sites passed the bare literal `0` against
   a body returning `std::size_t`, so `R` deduced to `int` and `return fn();`
   narrowed **the success value, not only the error value**. All four bodies
   return a `reinterpret_cast<std::size_t>` of a live COM pointer:
   `create_shader_sig` (the >=11.1 VS/PS/GS create dwm uses),
   `create_tess_shader_sig`, `open_ddi_texture2d`,
   `create_ddi_scanout_texture2d`. Before T7 each was a hand-written body
   returning that cast straight out of its own `std::size_t` function — no
   conversion, no defect. Nothing warns: `-Wconversion` / `-Wshorten-64-to-32`
   are off, and the ~115 clang warnings a UMD build emits are exactly why
   `tools/umd-check.ps1` exists.

   ★ **The truncation was VISIBLE in the line 7k quoted as healthy.** Same dwm
   role, same shaders, same lengths:

   ```
   T6 (renders):  create_vertex_shader_11_1 ok: raw=0x1cd520fc300 len=4600 …
                                             …raw=0x1cd520fd200 len=180
   T7 (crashes):  create_vertex_shader_11_1 ok: raw=0x7bdb1800   len=4600 …
                                             …raw=0x7bdb2200   len=180
   ```

   `0x7bdb2200` is the low 32 bits of a Win64 heap pointer. `store_raw_com`
   stored it and `VSSetShader` AddRef'd it — `Fault offset: 0x8068c`, exactly.
   **A `raw=` that is 8 hex digits where its siblings are 11 is a truncated
   pointer, not a small allocation.** (Caveat: genuinely small `raw=` values do
   occur — 32-bit/WOW64 processes log them on T6 too. The comparison that means
   something is *within one process role*, not across the log.)

   **The fix is the static_assert, not the four `std::size_t(0)`s:**
   `static_assert(std::is_same_v<R, decltype(fn())>)` makes a sentinel whose
   type differs from the body's return type a compile error. Fault-injected on
   the Linux host — restoring one bare `0` fails with `'int' is not the same as
   'long unsigned int'`, exit 1 — via a two-file oracle that copies the template
   verbatim and compiles all thirteen call-site shapes. No VM needed.

   ⚠⚠ **THE PROCESS LESSON, and it is worse than 7k recorded: on the direct
   primary, A PICTURE ON SCREEN DOES NOT PROVE THE COMPOSITOR IS ALIVE.** The
   bisect row "T6 KMD + T7 UMD rendered fine for ~5 min (warm)" was never a
   pass. `Application` id 1000 for 04:30–05:30 holds **102 faults**, of which
   ~60 are `dwm.exe <- helios_umd_3b704b27b42a3ef1.dll` on a **~34-second
   cadence** for the entire hour, plus every probe exe that ran. dwm was
   crash-looping the whole time; the KMD kept scanning out the last composited
   buffer, so the desktop looked static-but-fine instead of black. Cold boot is
   black only because dwm never produces a first frame. **Check
   `Get-WinEvent -Id 1000` before calling any display observation a pass.** The
   same window's DXBC A/B returned **5 pairs where a healthy run returns 9** —
   the four missing ones were the tess/11.1 probes dying — and that visible
   shortfall was read as "bit-identical".

   **T7 GATE — the full run nobody had done, all on the one cold boot:**

   | Item | Result |
   |---|---|
   | Cold boot | `CM_PROB_NONE` first try, **no boot loop**; dwm and LogonUI survive |
   | `helios_umd.dll` faults since boot | **0** |
   | Desktop | `helios_paintcap` ×2 — full desktop, wallpaper, taskbar, clock matching the capture minute |
   | `kmd-gate-surface.ps1` | **CLEAN, exit 0**; `VpSA=1 ScSet=1 ScPch=7680 DspMd=124257286` identical to 7i/7k; `ScanoutDiag` absent |
   | `umd-gate-surface.ps1` | **CLEAN, exit 0**; eleven refusal counters; must-not-appear all clear |
   | D3D11 suites | knob suite `TOTAL failures=0`; extra suite every `rc=0`, `xproc_read_rc=0` |
   | DXBC A/B vs `dxbc-t6.txt` | **9/9 pairs byte-identical**, spanning all three container wrappers |
   | Ownership soak 300/10000 | device 300 = 1944, **5.99 handles/device** (T6: 1947 / 5.99), resource phase **flat at 1972** all ten samples, modules **+0**, failures **0/0**, dwm handles **−2** |
   | Fire Strike | Graphics **20269** (GT1 137.84, GT2 64.77, Physics 33697, Combined 5300, Overall 16577), **duration 383 s** — a real run, inside the T4a–T6 spread 19460–20473 |
   | DComp probe, A/B same session | fixed **1308 / 1360 / 1305** vs T6 backup **1195 / 1308 / 1110**, all `PROBE PASS` — T6's own spread is wider than the gap (7h) |
   | `present-gate:` cold-boot dwm | `n=5120 avg_us=1867 max_us=2820 timeouts=26 failed=0 noctx=0` |
   | System log | no bugcheck (last 25-07, pre-T7), no dump written, **no TDR 4101** |

   The printed `OWNERSHIP SOAK FAIL` is the pre-existing 6-handles-per-device
   teardown leak (7d(b)) failing the literal "flat" test, identically to T5/T6.
   The 8 `Application` id 1000 faults this boot are all `vulkan_virtio-*.dll`,
   provoked by the two `restart-device` deploys — **WS1 defect 0z,
   pre-existing.** `Kernel-Power` id 41 at boot is routine on this VM (it is
   logged on every reboot, 26-07 and 27-07 included), not a Helios bugcheck.

   `alloc_meta_format_unknown` reads 2–3 on some processes. It is a **T7-new**
   counter naming a pre-existing silent `D3DDDIFMT_UNKNOWN` allocation-meta
   downgrade (R1010 commit 2), so it has no T6 comparand by construction; the
   gate script judges the surface clean. Left as an owed observation, not a
   regression.

   **The box is left on KMD 22.22.189.0 + UMD `ba6adde3`, rendering.**


7m. **T8 IMPLEMENTED — the LAST tranche of the Phase-1 refactor review. 49
   commits, KMD 22.22.190.0. GATE NOT YET RUN (needs a cold boot).**

   Eight items, R1101–R1108, plus M1/M2. Every file the review names is split.
   The headline numbers, all measured after `cargo fmt`:

   | File | Before | After |
   |---|---|---|
   | `umd/src/forward.rs` | 10744 | **562** + 16 modules under `forward/` |
   | `kmd_render/src/virtio/venus.rs` | 5603 | **470** (`venus/mod.rs`) + 7 |
   | `kmd_render/src/virtio/gpu.rs` | 3289 | **2346** (`gpu/mod.rs`) + 3 |
   | `kmd_render/src/adapter.rs` | 2479 | **1341** (`adapter/mod.rs`) + 5 |
   | `umd/bridge/dxvk_bridge.cpp` | 2217 | **1338** + 2 TUs + 3 headers |
   | `umd/src/lib.rs` | 1020 | **53** + 5 modules |
   | `kmd_render/src/ddi/start_device.rs` | 1069 | **deleted**, 4 modules |

   **Verified state (host + VM `cargo check`, no boot yet):** KMD 0 errors /
   **3 warnings**; release UMD 0 errors / **14 warnings, 2 rustc** — both the
   7l baseline exactly. `kmd_logic` **46 tests** green. `cargo fmt --check`
   **zero diffs** in all four crates. Deepest boot chain **17488 B, headroom
   448** — 96 bytes BETTER than 7l's 17584/352, because `setup_bar_segment`
   and `build_segment_table` left `dxgkddi_start_device`'s module.

   **How "move-only" was proved rather than asserted.** A multiset line diff,
   with leading visibility keywords normalised away so a sanctioned widening
   is not counted as a change, run per item against a pristine copy of the
   pre-split file. Every item reports **LOST (0)** — including R1107's
   **9959 lines across 14 modules**. The handful of lines that did report as
   lost are each named in their commit message and are all `use` fixups, the
   scheduled M2 doc rewrite, or the `super::gpu::` → `crate::virtio::gpu::`
   path changes a module move forces.

   **Cheap oracles that replaced VM runs, all fault-injected to prove they
   bite:**
   - `venus/ring.rs` — SHA-256 of `store_u32_seqcst`, `write_ring_buffer`,
     `load_u32_acquire`, `publish_and_notify`, `ring_wait_until` matches
     pre-split; whole-file `fence`/`compiler_fence` sequence unchanged
     `[Acquire, SeqCst, SeqCst, compiler_fence(Release)]`.
   - `venus/bringup.rs` — the `diag()` multiset is identical (35 = 35) and the
     bring-up ladder appears in the pristine ORDER, so the `0x0D00_0001` …
     `0x0D00_000D` sequence cannot have been reordered.
   - `bridge_dxbc.cpp` / `bridge_icd_exports.cpp` — all nineteen sampled
     bodies hash-match the pre-split file.
   - **`tools/fl-profile-oracle.rs` (NEW)** — replays the eight pre-R1106
     `feature_level_mode()` predicates against the new `FeatureProfile` for
     knob values 0..6: all identical. Pointing the `2 =>` arm at `FL11_0`
     makes it fail, exit 101.
   - R420's `#![deny(deprecated)]` log guard re-proven by injecting a direct
     `crate::log_line(..)` call: hard compile error, as designed.
   - The six `#[no_mangle]` exports checked against the built DLL's export
     table with `llvm-readobj --coff-exports`, not by inspection.

   **THREE THINGS DROPPED, each with evidence, per the tranche's own rule
   ("if an item cannot be made move-only, say so and drop it"):**

   1. **R1103's three field-disjoint sub-structs.** `ResourceTables` IS
      disjoint (its 10 fields are touched by exactly 32 methods, no
      `self.<method>()` call crosses the boundary either way) — but making it
      a struct costs ~130 lines of hand-written delegation in the blob/window
      allocator, which is new code in a tranche that writes none, against a
      defect class that has never fired. `CtrlQueue`+`FenceTables` cannot be
      done as specified at all: the review names THREE methods needing a
      hoist, measurement finds **SIX** — `drain_used`,
      `note_wddm_submission`, `note_scanout_refresh` (predicted) plus
      `latch_failed_and_fail_inflight`, `fence_wait_prepare`,
      `fence_event_register` (NOT predicted) — while `async_retired_up_to`,
      which the review lists, touches only `inflight` and needs no hoist. All
      six sit on the completion path. **Owed: its own tranche and gate.**
   2. **R1105's `bridge_flip_wait.cpp`.** Nothing to move — R912(a) retired
      the kwait subsystem, so `HeliosCbSignalSyncFromCpu`, `HeliosFlipWaitCtx`,
      the 64-slot latency ring, `present_flip_wait_setup`/`_arm` and
      `HeliosVenusScanoutInfo` do not exist.
   3. **R1108's TLS sealing.** `dxgi_present` touches the vehicle cell at FOUR
      sites and none is a read (an arm-and-consume `match` with per-arm side
      effects, two `set(Idle)` failure resets, one `set(Minted{..})` publish).
      Wrapping them is API design in the per-frame path. `VEHICLE` is
      `pub(crate)`. **Owed: `take_present_source()` + those four call sites.**

   **Corrections to `REFACTOR_REVIEW.md`'s T8 section found while
   implementing (thirteen):**
   - `Writer` is NOT moved to `venus/protocol.rs` — T0 already put it in
     `kmd_logic` with **seven** host tests, so the item's "add the T0-style
     `Writer` test" is already satisfied.
   - **M1 needs no commit**: the `fatal` field doc already states the PASSIVE
     sleep-poll reality and keeps the wedge rationale.
   - **M2's first paragraph needs no commit**: the `QueryDeviceDescriptor` doc
     already describes serving `adapter.edid`. Only the `DxgkDdiResetDevice`
     "no hardware to quiesce until Phase 2" paragraph was stale; rewritten.
   - `VENUS_ALLOC_ENABLED` (R1102) no longer exists.
   - `ADAPTER_COOKIE` (R1106) does not exist; it is `AdapterToken`/
     `ADAPTER_TOKEN`, a ZST whose ADDRESS is the handle.
   - R1106's knob module takes **four** readers, not six —
     `vehicle_kernel_flip_wait` and `present_sync_publish_enabled` went with
     T6/R912.
   - `FeatureProfile` has **five** fields, not six: there is no second levels
     cap, `D3D11DDICAPS_3DPIPELINESUPPORT` IS the levels bitmask.
   - **`forward/format.rs` NOT created.** T7/R1010 moved the eight DXGI
     classification tables to `umd/src/format.rs`; what is left in
     `forward.rs` is eight three-line delegating wrappers, so the module would
     delegate to the module that owns the subject.
   - R1108: **one** TLS cell (`Cell<VehicleSlot>`), not three — the
     `Idle`/`Armed`/`Minted` states replaced `PRESENT_SOURCE`,
     `LAST_VEHICLE_DEVICE` and `PRESENT_RESULT`.
   - `tools/kmd-frame-sizes.ps1` matched the boot symbol by the mangled
     substring `12start_device20dxgkddi_start_device`; R1102's module rename
     changed the length prefix, so the symbol stopped being found. Retargeted.
   - Two **pre-existing merged doc blocks** found and fixed: `init_hpd`'s
     summary sat above `reset_display_publication_state`, and
     `bring_up_venus`'s entire nine-line `#[inline(never)]` STACK BUDGET
     rationale sat above `zero_linear_scanout_breadcrumbs` — leaving
     `bring_up_venus` with no doc at all.
   - `adapter/segments.rs` carried a **broken doc link** to
     `crate::ddi::start_device::BAR_SEGMENT_ID`, a constant deleted when the
     id-3 topologies went. Retargeted to `crate::ddi::gpummu::MEMORY_SEGMENT_ID`.
   - The tree was **not `cargo fmt` clean before T8**: 191 hunks at `f60febc`
     (kmd_render 69, umd 102, kmd_logic 19, protocol 1). Most of the fmt
     commit is that pre-existing drift.

   **Two modules added beyond the review's list**, both recorded in their
   commits: `forward/tiles.rs` (the self-contained WDDM1.3 sparse-resource
   subsystem) and `forward/layout.rs` (input layouts + the VS input-variant
   cache, of which `bind_input_layout` alone is 135 lines).

   ⚠ **The 37 `LogThrottle` statics are byte-identical as a set** — 37 before,
   37 after, `diff` of the sorted names empty. That is the item's explicit
   constraint and it matters because eleven are SHARED by sites with different
   log budgets, so renaming or renumbering one changes a cadence.

   **NEXT: the T8 gate.** Needs a cold boot; nothing has been deployed. The
   box is still on KMD 22.22.189.0 + UMD `ba6adde3`.


7n. **T8 GATE — PASSED on one cold boot (2026-07-28 15:39:45), KMD
   22.22.190.0 + UMD `DB343F02…`.** Owner scoped it to everything except the
   one-hour DWM-crash soak; DOOM was dropped on owner instruction. Two gate
   lines are NOT OBTAINABLE on this box and are recorded as such below, not
   as skips.

   | Item | Result |
   |---|---|
   | Cold boot | **`CM_PROB_NONE` first try, no boot loop** — the real risk, since T8 rewrote `DxgkDdiStartDevice`'s module |
   | `helios_umd.dll` faults since boot | **0** — across Fire Strike, both D3D11 suites, the shader probes, 4 DComp runs, the soak, restart-device, a DWM restart and a 60 s idle |
   | Desktop | `helios_paintcap` ×3 — full desktop, wallpaper, taskbar, clock matching the capture minute each time (15:41, 16:07, 16:10) |
   | UMD actually loaded | DriverStore `..._521ab82e85c3fc2b`, SHA-256 `DB343F02…` — matched against the installed file, not assumed |
   | `kmd-gate-surface.ps1` | **CLEAN, exit 0**, twice (first boot and after the whole chain). `VpSA=1 ScSet=1 ScPch=7680 DspMd=124257286`, identical to 7i/7k/7l; `ScanoutDiag` absent |
   | `umd-gate-surface.ps1` | **CLEAN, exit 0**, twice; eleven refusal counters, must-not-appear all clear |
   | T7-critical breadcrumbs | **all identical to 7l**: `SdgDevX=1 SdgDevR=0`, `SdgLStg=16 SdgLReq=7910400 SdgLBit=15 SdgLTyc=5 SdgLPch=7680`, `BarF=28 BarB=0`, `SdgM=0`, `IrqlBad=0`; `PHQcall`/`HwQRef`/`RfUnb`/`VnEncOvf`/`CpImgVr`/`CpMemVr`/`PBBufVr` absent |
   | D3D11 suites | knob suite **`TOTAL failures=0`**; extra suite every **`rc=0`**, **`xproc_read_rc=0`** |
   | DXBC A/B vs `dxbc-t7fix.txt` | **9/9 pairs, byte-identical** — and identical to `dxbc-t6.txt` too. The R1105 C++ TU split changed nothing in the container synthesis |
   | Fire Strike | Graphics **20058** (Physics 33689, Combined 5391, Overall 16558), **duration 377 s** — a real run, inside the 19460–20473 spread, between T6 (20003) and T7 (20269) |
   | DComp probe ×3 | **1247 / 1343 / 1257**, all `PROBE PASS` (T7 1308/1360/1305; T6 backup 1195/1308/1110) |
   | Ownership soak 300/10000 | device 300 = 1946, **5.99 handles/device** (T6 5.99, T7 5.99), resource phase **flat at 1974** all ten samples, modules **+0**, failures **0/0**, dwm handles **+0** |
   | Stack frames | **17488 B, headroom 448** — 96 B BETTER than 7l's 17584/352, and unchanged by `cargo fmt` |
   | `pnputil /restart-device` | completes, `CM_PROB_NONE`, desktop recovers |
   | Deliberate DWM restart | pid 5032 → 7136, desktop recovers |
   | Idle-to-active wake | **0.65 s** after a 60 s idle (T0-era reference 1.1 s) |
   | System log | no bugcheck, no dump, **no TDR 4101** |

   The printed `OWNERSHIP SOAK FAIL` is the pre-existing 6-handles-per-device
   teardown leak (7d(b)) failing the literal "flat" test, identically to
   T5/T6/T7 — the per-device RATE is what matters and it is unchanged at 5.99.

   The only 4 `Application` id-1000 faults on the whole boot are
   `vulkan_virtio-41a8bda2401f.dll` in dwm, Explorer, SearchHost and
   ApplicationFrameHost, all at 16:07:16 — provoked by the one
   `restart-device`. **WS1 defect 0z, pre-existing.** `helios_umd.dll`: zero.

   `srv_raw_hazard=1` and `discard_partial=1..10` read non-zero; the gate
   script judges the surface clean and these are ordinary DDI-shape refusals,
   not new. `alloc_meta_format_unknown` is **0** this boot (it read 2–3 in 7l).

   **⚠ TWO GATE LINES ARE NOT OBTAINABLE ON THIS BOX — recorded, not skipped:**

   1. **Suspend/resume: impossible.** `powercfg /a` reports S1, S2, S3,
      Hibernate, S0-Low-Power-Idle and Hybrid Sleep ALL unavailable — "the
      system firmware does not support this standby state" for every one. No
      driver change can make this testable; it needs a VM firmware/machine-type
      change, which is owner territory. This also means the same-context PnP
      stop/start carry-over path (`StRst`, `RfUnb`) STILL has no way to be
      provoked on this box — `restart-device` re-runs AddDevice with a fresh
      zeroed context, exactly as T1a/7i and the 46th recorded. `StRst=0`
      `StRstR=0` this boot are therefore "never exercised", not "exercised and
      clean".
   2. **Same-boot QEMU scanout evidence: unavailable on this launcher config.**
      The VM is running `HELIOS_DISPLAY=sdl` (confirmed from
      `/proc/<qemu>/cmdline`), so there is no VNC endpoint and the host-side
      `vulkan-readback: OPTIMAL DMA-BUF ready` path is never driven — the last
      such line in `/tmp/helios-qemu-stderr.log` is **10:23:37, before this
      15:39:45 boot**. Producing it needs a relaunch with `HELIOS_DISPLAY=egl-vnc`,
      which CLAUDE.md puts under owner control. The same-boot host-path evidence
      that IS available is the KMD's `ScSet=1` (SET_SCANOUT_BLOB accepted),
      `ScFlu=3`, `ScPch=7680` and `ScanoutDiag` absent, plus a visibly
      composited desktop. Also re-confirmed while checking: the host log's
      `OPTIMAL DMA-BUF shape mismatch` line is **PRE-EXISTING**, first
      occurrence 2026-07-26T21:41:56, 106 of them against 326 successes.

   **Not run, by owner decision:** the one-hour mixed session, and DOOM.

   **The box is left on KMD 22.22.190.0 + UMD `DB343F02…`, rendering.**
   Backups: `C:\Users\Rupansh\helios-umd-backup-t7.dll` (the `ba6adde3` T7
   UMD), `...-t6.dll`, `...-t5.dll`, `...-t4b.dll`;
   `C:\ProgramData\HeliosDeployBackups\20260728-153923` is the pre-T8
   DriverStore.

   **This closes the Phase-1 refactor review: T0–T8 are all landed and gated.**

---

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
