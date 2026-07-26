# Phase-2 handoff prompt — implement the KMD/UMD quality refactor

Paste the block below into a fresh session.

---

You are the primary implementor on **Helios vGPU** (`/home/rupansh/helios-vgpu`, branch `wddm`) —
a Windows 11 WDDM 3.2 render+display miniport (`kmd_render/`, Rust `no_std`) speaking Venus over
virtio-gpu, plus a D3D11 UMD (`umd/`, d3d10umddi frontend bridged via cxx to a forked DXVK).
Read `CLAUDE.md` first; its invariants and operating rules override any default behavior.

## Your task

Execute **phase 2** of `REFACTOR_HANDOFF.md`: implement the accepted recommendations in
`REFACTOR_REVIEW.md` (repo root, ~7150 lines). Phase 1 — the adversarial review — is complete;
do not redo it. Phase 3 (build, deploy, regression-test) is interleaved: each tranche has its own
gate and you run it before starting the next tranche, not at the end.

`REFACTOR_REVIEW.md` contains 177 recommendations from 300 verified findings, in eleven
dependency-ordered tranches. Start by reading §1–§4 (method, what must not change, verdict,
implementation order), then the tranche you are about to work. §6 holds two cross-cutting
registers you will consult repeatedly: the **implicit-ordering register** and the
**static-guarantee catalogue**. Appendix A holds the per-tranche gates, Appendix E the working
rules, Appendix D the three refuted findings and one residual hazard.

Do **not** read `docs/archive/**`, and do not go looking in git history for the earlier
`REFACTOR_REVIEW.md` or `PHASE2_HANDOFF_PROMPT.md` from commit `8d2fe3f` — that review was
deliberately superseded by a from-scratch one and reconciling against it will only mislead you.

## Order of work

Tranche order is not by severity; it is by what makes the next tranche safe.

| Tranche | What it is | Deploy cost |
|---|---|---|
| **T0** | Make the refactor verifiable and the deploy chain honest (5 recs) | host tests + package check, then one reboot |
| **T1a** | KMD wedge-class bug fixes: display gate, lifecycle reset, transport latches (16) | KMD image + reboot |
| **T1b** | KMD validation/status-contract fixes + the per-`Present` registry-write storm (30) | KMD image + reboot |
| **T2** | UMD and bridge bug fixes + the hot-path logger and per-present probe cost (30) | release UMD + adapter restart |
| **T3** | Encode the display/scanout invariants (17) | KMD image + reboot |
| **T4a** | Encode the transport, venus and allocation invariants (30) | KMD image + reboot |
| **T4b** | Encode the caps, segment-topology, paging and handle invariants (22) | KMD image + reboot |
| **T5** | Encode the UMD/bridge invariants: handles, descriptors, FFI ownership (29) | release UMD + adapter restart |
| **T6** | Delete the proven-dead paths (18) | KMD image + reboot |
| **T7** | De-duplicate and consolidate (17) | KMD image + reboot |
| **T8** | Split the large files, move-only (8) | KMD image + reboot |

**Start with T0, and do not skip it.** It is not a refactor. `kmd_render` and `umd` currently have
**zero unit tests** (only `protocol/src/virtio_gpu.rs` has three), so every "behavior preserved"
claim in T1a–T8 would otherwise rest on booting the guest and eyeballing the desktop. T0 also fixes
two things that would corrupt your own measurements: `kmd_render/Cargo.make.toml:24-48` hardcodes
`target/debug/helios_umd.dll`, so a default deploy ships and measures the **debug** UMD; and the
KMD version lives in five hand-edited literals whose coherence check lives outside the source tree
(a mismatch is `FAILED_ADD 0xc0000182` at install).

T2 and T5 are UMD-only and need no guest reboot, so interleave them with KMD work when a reboot is
inconvenient. Tranches may be paused between any two entries — each is independently valuable.

## Rules that are not negotiable

1. **One recommendation per commit**, unless its *Atomic boundary* line lists ordered sub-commits —
   in which case that order is load-bearing.
2. **Never fold a `BUG` fix into a structure move.** The `BUG` items change behavior on purpose and
   need their own commits so a bisect can find them. A behavior change hidden inside a file move is
   the exact failure mode this review exists to prevent.
3. **Re-read the cited lines before editing.** Line numbers are correct as of commit `8d2fe3f` and
   drift as tranches land. Every recommendation names symbols as well as lines — trust the symbol.
4. **When the review and the code disagree, the code wins.** Say so and adjust the recommendation;
   do not edit the code to match the review. Record the correction in the entry.
5. **Behavior preservation is the contract** for everything that is not marked `BUG`. Preserve the
   direct primary, completion ordering, loud-failure contracts, registry knob names, counter names,
   and diagnostic breadcrumb values. Several recommendations *do* migrate names — each says so and
   states old and new. Never change one silently.
6. **Bounded timeout around a real event/fence wait = a safety contract; keep it.** An arbitrary
   delay used to make ordering look correct = a hack; remove it. The review distinguishes these
   case by case — the UMD's bounded 10 ms condition-variable frame gate is the former and stays.
7. **No panics on kernel release paths.** No `panic!`/`todo!`/`unwrap`/`expect`, no indexing or
   arithmetic that can panic. A panic in a DDI is a silent graphics deadlock. Several
   recommendations exist purely to remove existing ones; do not add any.
8. **Every skipped or refused path increments a named counter.** Loud failure over fake success. Note
   the trap the review found: `diag::record` returns early at the default `DiagLevel`, so a
   breadcrumb routed through it is **invisible in production** — use the ungated named-counter path
   for anything that must be evidence.
9. **Run the tranche gate before starting the next tranche.** Registry counter values persist across
   boots: verify a counter *moves* this boot before trusting it. Presence is not evidence.
10. Only user-visible desktop state counts as rendering evidence (`helios_paintcap` →
    `Z:\tmp\screen_copy.png`). Log lines are not frames.

## Build, deploy, verify

Use the **`win` MCP server** — never raw SSH, and never touch `~/.ssh`. `win_build_kmd` (bumps the
version sites with a coherence check, then cargo-make) → `win_install_kmd`. For UMD-only work:
`win_cargo` then `win_install_umd -UmdDll ...\target\release\helios_umd.dll` (it defaults to the
**debug** DLL — pass release explicitly) then `pnputil /restart-device`.

`CARGO_TARGET_DIR` must be per-platform: Linux `target/linux`; Windows a **local `C:` path**, never
`Z:\` (Rust file IO fails on the 9p share with OS error 87). See `TOOLCHAIN.md`.

A newly built KMD image requires a guest reboot; a UMD-only change needs only an adapter restart.
**Guest reboots are disruptive — ask the owner before requesting one**, and batch KMD work so you
need fewer of them. Also ask before changing `tools/launch-helios-gtk.sh`, the QEMU display/debug
transport, or launcher environment variables — then stop and ask the owner to restart the VM.

The standing regression surface, per Appendix A: KMD + release UMD builds and formatting/diff
checks; healthy Helios device state and expected driver/UMD binding; `ScanoutDiag` **absent** with
`VpSA=1` and `ScSet=1`; visible desktop; idle-to-active responsiveness; rapid cursor motion with no
trails; no unprompted DWM crash; no new present-gate steady-state timeouts, control timeouts, or
ring failures; DComp present cadence near the 63 fps baseline; and same-boot QEMU evidence for the
actual OPTIMAL DWM primary (`/tmp/helios-qemu-stderr.log`), not a diagnostic fill image.

`ROADMAP.md` §Tooling is the live inventory of knobs, counters, probes, and the ETW `AzureTriage`
recipe. `BRINGUP_QUIRKS.md` has the deploy/VM gotchas. `NTOSEYE.md` covers live KD.

## Reporting and record-keeping

- Track per-entry status inside `REFACTOR_REVIEW.md` itself (LANDED / ADJUSTED / DEFERRED /
  WITHDRAWN, with a one-line reason). It is the living document for this workstream.
- Update `ROADMAP.md` as items close or new defects appear.
- Distil session state into agent memory. Do **not** create per-session `HANDOFF_*.md` docs.
- Land perf changes with before/after numbers, measured — not asserted. Measure present-to-scanout
  and VNC delivery separately, and measure wake latency separately from steady-state cadence.
- Flag blockers proactively. Ask the owner on fundamental architecture questions rather than
  guessing; the review names the places where a decision is genuinely the owner's.

## Where to be careful

The review's own risk notes, worth internalising before you start:

- **T3 and T4a carry the highest blast radius.** They encode the display/scanout and
  transport/venus invariants, i.e. exactly the paths that make the desktop visible. Their gates
  require capturing per-flip diag values for a fixed session *before and after* and diffing them.
- **T6 deletes code.** Every removal candidate in the review carries the evidence that proves it
  unreachable. Anything the verifiers could only mark *suspected* must be proven before deletion,
  not deleted on the strength of the review.
- **T8 is move-only.** If a split tempts you into a semantic change, stop — that temptation is what
  the tranche ordering exists to defuse.
- 21 findings remain marked `suspected` because confirming them needs a live guest or a host trace.
  They are called out in place. Do not action them on the review alone.
- Appendix D records one *future-edit trap*: `enqueue_sync` rings the virtqueue doorbell before
  publishing its in-flight entry (`kmd_render/src/virtio/gpu.rs:1101-1115`), the opposite of its
  sibling `enqueue_async_control` which documents publish-then-notify at `:1190-1191`. It is safe
  today **only** because every `drain_used` call site and the sole `enqueue_sync` caller
  (`ctrl.rs:257-262`) hold `virtio_lock`. If any tranche narrows that lock, moves the doorbell out
  of the critical section, or adds an `enqueue_sync` caller outside `with_virtio`, the race becomes
  live with no compiler signal.

Never blame the host stack (proven good) without host-side evidence. Every Helios failure is a
guest-side bug until host evidence says otherwise.
