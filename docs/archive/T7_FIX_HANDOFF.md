> **RESOLVED 2026-07-28 by `ead692e`. This brief is history — read `ROADMAP.md`
> 7l instead.** The defect was in NONE of the three candidates below: it was
> `bridge_guard` (R1014 commit 4, `919f28a`) deducing `R = int` from a bare `0`
> sentinel and truncating every `std::size_t` return — including
> `create_shader_sig`'s live COM pointer — to 32 bits. The whole T7 gate has
> since passed on a cold boot. Two claims below are also now known to be FALSE:
> the bisect's third row was never a pass (dwm was crash-looping behind a stale
> primary), and the T7 DXBC A/B it calls "bit-identical" returned 5 pairs where
> a healthy run returns 9.

# HANDOFF: fix the T7 UMD crash (dwm + LogonUI, 0xc0000005, black screen)

Branch `wddm`, HEAD `ccbd2c1`. **24 commits on top of `f1c6ace` (T6).** Working
tree clean.

Your job is ONE defect. T7 is otherwise complete and its KMD half is verified.
**Do not re-do T7. Do not re-litigate its dispositions. Find and fix the crash.**

---

## Read first, in this order

1. **`ROADMAP.md` 7k** — the gate result and the bisect. Authoritative.
2. **`ROADMAP.md` 7j** — what T7 implemented, and the nine scope corrections to
   the review's T7 section. You will need 7j to know which commit owns what.
3. `ROADMAP.md` 7i — the T6 gate, which is T7's before-baseline.
4. The 51st memory (`t7-umd-crash-handoff-51st`).

Do **not** re-derive the item list from `REFACTOR_REVIEW.md`. Its T7 section is
stale in nine verified ways; 7j lists them.

---

## The state of the box RIGHT NOW

**Working.** Do not "fix" it before you have read this.

| | |
|---|---|
| KMD | **22.22.189.0** (T7), DriverStore `..._f4e03b638c5dcc2c`, `CM_PROB_NONE` |
| UMD | **T6 `355b4366b1666104`**, from `C:\Users\Rupansh\helios-umd-backup-t6.dll` |
| Boot | 2026-07-28 12:22:12 |
| Display | renders (owner-confirmed) |

Backups you will need:

```
C:\Users\Rupansh\helios-umd-backup-t6.dll        355B4366… the WORKING UMD
C:\Users\Rupansh\helios-umd-backup-t5.dll
C:\Users\Rupansh\helios-umd-backup-t4b.dll
C:\ProgramData\HeliosDeployBackups\20260728-122106   pre-T7-KMD DriverStore files
```

The broken T7 UMD is at `C:\Users\Rupansh\helios-vgpu\umd\target\release\helios_umd.dll`,
hash `3B704B27B42A3EF1195F5FFE0D516040C661AC14C675084FCFBE36598F38DF96`.
Rebuild it from source with `tools/umd-check.ps1 -Mode release` (it builds into
the **MIRROR**, `C:\Users\Rupansh\helios-vgpu\umd\target\release` — always pass
`umd_dll` explicitly to `win_install_umd` with that ABSOLUTE path, or the tool
resolves it against `Z:\` and fails).

---

## The defect

`dwm.exe` and `LogonUI.exe` crash-loop with `0xc0000005` inside
`helios_umd.dll`. No compositor survives, so the QEMU window is black.

**★ It is ONE deterministic site.** Every crash, in both processes, reports the
same `Fault offset: 0x000000000008068c`. Resolved against the release PDB
(ImageBase `0x180000000`, so VA `0x18008068c`):

```
llvm-symbolizer --obj=helios_umd.dll --demangle --functions=linkage 0x18008068c

  std::_Atomic_integral<unsigned int,4>::operator++            atomic:1469
  dxvk::ComObject<ID3D11VertexShader>::AddRefPrivate           com_object.h:59
  dxvk::ComRef_<D3D11Shader<ID3D11VertexShader,…>>::incRef     com_pointer.h:37
  dxvk::Com<D3D11Shader<ID3D11VertexShader,…>>::operator=      com_pointer.h:76
  dxvk::D3D11CommonContext<D3D11ImmediateContext>::VSSetShader d3d11_context.cpp:1397
```

**`VSSetShader` incrementing the refcount of a bad `ID3D11VertexShader`
pointer.** A null would not fault there (DXVK handles null); this is a non-null
pointer that is not a live `D3D11Shader`.

The last UMD log line before a crash (dwm pid 3080, `C:\ProgramData\Helios\umd-3080.log`):

```
DDI create_vertex_shader_11_1 ok: raw=0x7bdb2200 len=180 sig_in=3 sig_out=3
DDI create_resource(buffer) ok: bytes=144 fmt=0 usage=2 bind=0x1 misc=0x0
```

So many shader creates succeed first; the fault is on a later **bind**.

### Bisect result, already done

| Stack | Result |
|---|---|
| T7 KMD + T7 UMD | crash-loop, black |
| T7 KMD + T6 UMD | **renders fine** |
| T6 KMD + T7 UMD | rendered fine for ~5 min — but **WARM ONLY** |

The KMD is exonerated. The third row is *not* evidence the UMD is fine cold: it
was a `pnputil /restart-device` into an already-logged-in session, so LogonUI
and a cold-boot device create were never exercised. **That is the process
mistake this handoff exists to correct.**

---

## The three candidates, in order

All three are UMD-only commits. Judge them on the `VSSetShader` evidence.

**1. `f3f33ea` — R1011, `stage_set_shader!`.** The prime suspect by locality:
it rewrote `vs_set_shader` itself, and `vs_set_shader` is the ONE member of the
family with an asymmetry — it also writes `ia.bound_vs_com`, read by the
input-variant recompiler. Check the macro expansion against the pre-change
bodies literally:

```
git show f3f33ea^:umd/src/forward.rs | sed -n '/unsafe extern "C" fn vs_set_shader/,/^}/p'
```

Specifically verify: the `(ComType, ContextMethod)` pairing per stage (a
mismatched pair mis-binds a stage silently — the review names this as the
item's own risk), and that the `RefCell` borrow scope did not widen. My macro
uses `let mut ia = …borrow_mut();` for ALL six where five originals used a
temporary `…borrow_mut().current_ps = com;`. Both drop before
`d3d11_context(h)`, so I believe it is equivalent — but re-derive it, do not
take my word.

**2. `0dc63e3` — R1016, `SigWords`.** Reached only through `bound_vs_com`, i.e.
only from the VS path. `create_vs_input_variant` calls
`dxvk.create_shader_sig(...)` and stores the result as a COM pointer. If that
now returns a stale or wrong `raw`, a later `VSSetShader` AddRefs freed memory —
which matches the symptom exactly. Check `replace_inputs` and
`set_input_comptype` against the original index arithmetic:

```
git show 0dc63e3^:umd/src/forward.rs | sed -n '/unsafe fn create_vs_input_variant/,/^}/p'
```

**3. `12c5097` — R1009, the device-funcs typestate.** If the fill or the install
order changed, the runtime holds a shader handle that never carried a real COM
pointer. Cheap to clear: `audit_wddm1_3_device_funcs` already prints a
real/noop/calc/null census and the nine named WDDM1.3 extension slots under
`UmdTrace=1`. Capture it on the T6 UMD and on the T7 UMD and diff. **Note the
three pre-change `calc!` lists were verified programmatically identical (18
entries each), so that half is already cleared.**

Everything else in the UMD half is further away: R1008 (knobs), R1010 (format
table, with a `0..=200` equivalence test that passes on the host), R1012 (view
shapes), R1013 (present tail), R1014 (bridge, and the DXBC containers were
proven bit-identical against the T6 UMD).

---

## How to bisect it properly

Each UMD commit is independently deployable. **No reboots needed for the
build/deploy loop — but the CRASH ONLY REPRODUCES COLD, so the verification step
does need one.** Ask the owner before each reboot.

```
# build one candidate
git checkout <sha> -- umd/           # or check out the commit
powershell -File Z:\tools\umd-check.ps1 -Mode release

# deploy (ABSOLUTE mirror path — this is load-bearing)
win_install_umd umd_dll:"C:\Users\Rupansh\helios-vgpu\umd\target\release\helios_umd.dll" \
                args:["-KillUmdUsers","-RestartDevice","-NoProbe"]

# reproduce: a restart-device is NOT sufficient. Reboot, then:
Get-WinEvent -FilterHashtable @{LogName='Application'; Id=1000; StartTime=(Get-Date).AddMinutes(-10)} |
  Where-Object { $_.Message -match 'helios_umd' }
```

A faster inner loop, if you can find a warm repro: nothing found one in this
session, but the fault is on a VS **bind**, so `helios_d3d11_knob_suite_default`
or `helios_triangle` may hit it warm. Try that before spending reboots.

**Recovering a black box**: SSH still works (the crash is per-process, not a
bugcheck). Deploy the T6 UMD backup and the display comes back without a reboot:

```
win_install_umd umd_dll:"C:\Users\Rupansh\helios-umd-backup-t6.dll" \
                args:["-KillUmdUsers","-RestartDevice","-NoProbe"]
```

---

## Constraints on the fix — READ THIS BEFORE YOU TOUCH ANYTHING

**"Ensure the fix does not introduce trash code" is the owner's explicit
instruction, and here is what that means concretely.**

- **Fix the defect, do not revert the item.** If R1011's macro is wrong, correct
  the macro; do not restore six copy-pasted bodies. The whole point of T7 is
  that `vs_set_shader`'s one asymmetry is visible instead of buried in fourteen
  identical lines. Reverting re-buries it.
- **One commit for the fix, scoped to the defect**, with the fault offset, the
  resolved symbol and the bisect in the message. Never fold it into a structure
  move.
- **No new knobs, no new stubs, no `#[allow]` to silence a warning, no
  commented-out code, no "temporary" instrumentation left in.** If you need a
  counter to find it, it goes in R911's `DdiRefusals` (now **eleven** fields —
  T7 added `alloc_meta_format_unknown` and `readback_stride_unsafe`) and
  `tools/umd-gate-surface.ps1` gets re-anchored in the same commit, or it comes
  out before you commit.
- **Never `panic!`/`unwrap`/`expect` on a DDI path.** `helios_umd` is
  `panic = "abort"`, so a panic in a DDI kills DWM — which is the failure mode
  you are debugging.
- **Do not widen scope.** T8 splits `forward.rs` (10.5k lines); you are not
  doing that. No `cargo fmt` (T8's gate criterion). No file moves.
- If the honest fix is that one R-item cannot be made byte-identical, **say so
  and drop that item**, with the evidence. A dropped item recorded in ROADMAP is
  worth more than a fix nobody can verify.

---

## What "verified" means this time

The T7 UMD was called verified on a warm restart-device and it was not. Do not
repeat that.

1. **Cold boot** with the fixed UMD. `Get-WinEvent` Application id 1000 shows
   **no `helios_umd.dll` faults**. Expect `vulkan_virtio-*.dll` faults if you
   provoke a restart-device — that is WS1 defect 0z, pre-existing.
2. **`helios_paintcap` → `Z:\tmp\screen_copy.png`, and LOOK AT IT.** Full
   desktop, wallpaper, taskbar, clock matching the capture minute. Log lines are
   not frames.
3. Then the rest of the T7 gate, which nobody has run yet:
   `tools/umd-gate-surface.ps1` (must be clean; eleven refusal counters), both
   D3D11 suites (`TOTAL failures=0`), `tools/helios-ownership-soak.ps1 300 10000`
   (compare **per-device**: T6 was 1947 @ device 300, 5.99 handles/device — the
   soak crashes deterministically between cycle 301 and 400, pre-existing, 7d(b)),
   Fire Strike (**check the run DURATION — ~6.3 min; a 61 s run that writes a
   result file with Graphics=0 is the T6 trap**), `present-gate:` from dwm's log,
   and the DComp probe **A/B'd against the backed-up T6 UMD in the same session**
   (the probe's spread is wider than any tranche's effect — 7h).
4. `tools/shader-dxbc-ab.ps1 -Label <x>` and diff against `Z:\tmp\dxbc-t6.txt`,
   which is already captured. Five common shaders came back bit-identical for
   T7; they must stay that way.

---

## Facts you will otherwise waste time rediscovering

- **The host log's `vulkan-readback: OPTIMAL DMA-BUF shape mismatch
  required=8773632 fd_size=7913472` is PRE-EXISTING.** It first appears
  2026-07-26T21:41:56 and occurs 94 times, alongside 284
  `OPTIMAL DMA-BUF ready 1896x1030` successes. It looks exactly like an R1002
  encoder regression. It is not one. `grep -n` for the FIRST occurrence before
  blaming a tranche for any host-side line.
- **The KMD half is verified and must not be re-examined**: no boot loop,
  `kmd-gate-surface.ps1` clean, every T7-critical breadcrumb identical to the T6
  gate (`SdgDevX=1 SdgDevR=0 SdgLStg=16 SdgLReq=7910400 SdgLBit=15 SdgLTyc=5
  SdgLPch=7680 BarF=28 BarB=0`, `CpImgVr`/`CpMemVr`/`PBBufVr` absent), frame
  sizes **byte-for-byte identical** to T6 (deepest chain 17584 / headroom 352).
- `tools/kmd-check.ps1` (new) reports the KMD's own rustc diagnostics.
  `win_build_kmd` builds the UMD too and its ~115 clang warnings push the KMD
  warning count off the top of the captured output. KMD baseline: **3 warnings**.
  UMD baseline: **14** (2 rustc, both pre-existing).
- `kmd_logic` has **46** host tests (`cd kmd_logic && CARGO_TARGET_DIR=../target/linux
  cargo test --offline`), nine of them R1002 golden bytes whose literals came
  from compiling the PRE-CHANGE encoders. `tools/format-table-check.rs` runs
  R1010's `0..=200` equivalence test on Linux with
  `rustc --test --edition 2021`.
- UMD logs are pid-keyed and **appended**, and Windows reuses pids. Anchor every
  read to the last `UMD module:` line.
- `win_exec` strips `$vars` and chokes on embedded escaped quotes — write a
  `.ps1` to `Z:\tmp\` and run it with `-ExecutionPolicy Bypass -File`.
- `win_build_kmd`'s output exceeds the MCP cap; grep the saved file.
- Drive the VM through the `win` MCP tools only, never raw ssh.
