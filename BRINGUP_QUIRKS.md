# BRINGUP_QUIRKS.md — Helios KMD build / deploy / VM-control gotchas

The non-obvious mechanics of iterating on `helios_kmd_render` against the live win11 guest.
Every item here cost real time to discover. Pairs with [NTOSEYE.md](NTOSEYE.md) (kernel
debugging). The win11 guest IS the build host — drive it via the **win MCP** (`win_cargo`,
`win_exec`), not raw ssh (see CLAUDE.md / TOOLCHAIN.md).

> **Architecture note (2026-07-11):** Helios is now the live render+display
> adapter and drives `SET_SCANOUT_BLOB` through `qemu-helios`. Sections describing
> Code 43 on a render-only adapter or IddCx/Looking Glass behavior are dated
> bring-up evidence, not safe assumptions for the current VM.

> Captured during the Step-2 GpuMmu bring-up (2026-06-18/19). See the
> `step2-gpummu-implemented` memory for the debugging narrative.

---

## 1. The build doesn't always recompile (sync/mtime quirk) — ALWAYS purge

`win_cargo` robocopy-mirrors `Z:\ → C:\Users\Rupansh\helios-vgpu` then builds in the mirror.
The mirror copies your edits (verified — `Select-String` the mirrored source to confirm), **but
cargo's incremental build sometimes does NOT recompile** the changed crate (Linux↔Windows clock
skew makes the mirrored source mtime look "not newer" than the cached artifact). Symptom: the
build log has **no `Compiling helios_kmd_render` line** and finishes in ~1 s — you just deployed
a STALE binary.

**Fix — purge the crate's fingerprint + outputs before every build, then `win_cargo make`:**
```
C:\Users\Rupansh\helios-vgpu\kmd_render\target\debug\.fingerprint\helios_kmd_render-*   (rmdir)
C:\Users\Rupansh\helios-vgpu\kmd_render\target\debug\deps\*helios_kmd_render*           (del)
C:\Users\Rupansh\helios-vgpu\kmd_render\target\debug\helios_kmd_render.{sys,dll}        (del)
```
Then confirm the fresh `deps\helios_kmd_render.dll` mtime advanced and its size changed if you
changed code. There is ONE target tree (`kmd_render\target`); win_cargo does not use a separate
`CARGO_TARGET_DIR` for `cargo make`.

## 2. cargo-make packaging signs a STALE binary — repackage by hand

After a fresh compile, `cargo make` often copies an **old** `.sys` into
`…\helios_kmd_render_package\` and signs THAT (the package `.sys` size won't match the fresh
`deps\helios_kmd_render.dll`). Don't trust the package output. Repackage manually from the
freshly-built cdylib:
```
copy  deps\helios_kmd_render.dll  →  package\helios_kmd_render.sys
signtool sign /s WDRTestCertStore /n WDRLocalTestCert /fd SHA256  package\helios_kmd_render.sys
inf2cat /driver:<package_dir> /os:10_x64 /uselocaltime
signtool sign /s WDRTestCertStore /n WDRLocalTestCert /fd SHA256  package\helios_kmd_render.cat
```
- Use the **full signtool/inf2cat paths** — note they are in DIFFERENT arch dirs:
  `…\10\bin\10.0.26100.0\x64\signtool.exe` but `…\10\bin\10.0.26100.0\**x86**\Inf2Cat.exe`
  (inf2cat ships x86-only; calling a nonexistent x64 inf2cat path fails SILENTLY — no .cat).
  A bare `& signtool …` (not on PATH) **fails SILENTLY** — the `.sys` comes out `NotSigned` and
  the driver won't load. ALWAYS verify: `(Get-AuthenticodeSignature <sys>).Status -eq 'Valid'`.
- Signing adds ~1.4 KB (a 66560-byte `.dll` → 68000-byte signed `.sys`).
- Cert: `WDRLocalTestCert`, thumbprint `BB44916FAFF199C0B9659CDB319394F6DF3D671E`, in
  `WDRTestCertStore`. Test-signing mode must be on (it is — the baseline driver loads).
- **Re-running `inf2cat` over a package that already has a SIGNED `.cat` produces a CORRUPT
  `.cat`** (`CryptCATOpen` → `0x0000000D ERROR_INVALID_DATA`; the driver then fails to load with
  `0xC000026C STATUS_DRIVER_UNABLE_TO_LOAD` → **Code 39**, and the diag ring is EMPTY because
  AddDevice never runs). Fix: **delete `package\helios_kmd_render.cat` first**, then run `inf2cat`
  (run it **standalone** — chaining `Remove-Item; inf2cat` in one pipeline races and inf2cat
  silently doesn't emit the `.cat`), then sign the `.cat`.
- **VERIFY the catalog actually covers the binary** with
  `signtool verify /pa /c <cat> <sys>` (must print "Successfully verified"). `Get-AuthenticodeSignature`
  only validates the `.cat`'s OWN signature, NOT that its hashes match the `.sys` — it reports
  `Valid` even for a catalog that doesn't cover the deployed binary. Also confirm
  `(Get-FileHash deployed.sys) -eq (Get-FileHash package.sys)` after the in-place copy.

## 3. Deploy = scripted, verified hotplug only

Do not manually copy KMD, UMD, or ICD files during normal iteration. Use the scripts below and fix
the scripts when a deployment edge case is found:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\install-helios-kmd.ps1 -PlanOnly
powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\install-helios-kmd.ps1

powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\hotplug-helios-umd.ps1 -PlanOnly
powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\hotplug-helios-umd.ps1

powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\install-helios-icd.ps1 -PlanOnly -NoSmoke
powershell -NoProfile -ExecutionPolicy Bypass -File Z:\tools\install-helios-icd.ps1
```

The scripts verify SHA256 after every copy and fail loudly instead of letting a stale binary become
the next graphics-debugging target. KMD install preserves the active UMD by default; pass
`-IncludeUmd` only when a package-wide replacement is intended. UMD hotplug defaults to the
ProgramData override (`C:\ProgramData\HeliosUmd\helios_umd.dll`) and rebinds the adapter. ICD
hotplug uses content-hashed ProgramData DLLs and an atomically written Khronos manifest.

The KMD script signs with/imports a **machine-store** `WDRLocalTestCert` fallback. This is important
when LoginUI/Explorer are crash-looping: CurrentUser certs may be unavailable, but an elevated
session-0 script can still use `LocalMachine\My`, `LocalMachine\Root`, and
`LocalMachine\TrustedPublisher`.

## 4. Reloading the driver / replaying bring-up

**★ PREFERRED for KD/debug iteration: PnP disable→enable replay (no reboot, ntoseye stays
attached).** This is RELIABLE if you poll the transitions — the old "it's flaky / a no-op" claim
was just not waiting for `disable` to settle before `enable`. It re-runs the FULL bring-up
(AddDevice→StartDevice→QueryAdapterInfo→CreateProcess→CreateContext→…→teardown; the diag ring
repopulates), which is everything except the boot-only `DpiFdoStartAdapter*` path — and crucially
it does **not** drop the kernel debugger, so you don't lose ntoseye + re-setup on every iteration.
Procedure (Helios instance id `PCI\VEN_1AF4&DEV_1050&…&0017`):
```powershell
Disable-PnpDevice -InstanceId $id -Confirm:$false
# WAIT until it settles, or the enable races a half-torn-down device:
while ((Get-PnpDeviceProperty $id DEVPKEY_Device_ProblemCode).Data -ne 22) { sleep 0.4 }  # 22 = CM_PROB_DISABLED
# (clear the diag ring here for a clean read)
Enable-PnpDevice  -InstanceId $id -Confirm:$false
while ((ring S-value count) -eq 0) { sleep 0.4 }   # bring-up re-ran when breadcrumbs reappear
```
- This is safe for Helios specifically because it's **render-only and sits at Code 43** — it is
  NOT the active display adapter, so disabling it does NOT trip the DWM teardown deadlock.
  (Disabling the *live display* adapter — the gpu-gl/IDD path — still HANGS; never do that.)
- **`Enable-PnpDevice` BLOCKS until the device starts or fails.** So if you've armed a KD
  breakpoint that halts the guest mid-bring-up, the whole VM freezes and the `win_exec` running
  `Enable-PnpDevice` hits its timeout and is killed — **this is expected, not an error**. The
  guest stays halted at your breakpoint; give the enable a short `timeout_secs` and then drive
  ntoseye (`status`/`wait_for_stop`). Verified 3/3 deterministic re-runs to ring=93 / Code 43.
- `shutdown /r` and `Restart-Computer -Force` **hang** on the gpu-gl teardown (they report
  success but the guest never goes down — check `LastBootUpTime`; "A system shutdown is in
  progress.(1115)" means it's stuck).
- **Full reboot (only when you need the boot path or a wedged guest) = host QMP `system_reset`**
  on `/tmp/helios-tpm/mon.sock`:
  ```python
  s=socket.socket(AF_UNIX); s.connect("/tmp/helios-tpm/mon.sock")
  recv(); send '{"execute":"qmp_capabilities"}'; recv()
  send '{"execute":"system_reset"}'   # → RESET event
  ```
  (Also `query-status` to check `running` vs paused/bugcheck.) Downside: a reboot drops ntoseye
  (it 404s → the user must reconnect) and a KD-attached boot breaks repeatedly on DbgPrints
  (resume past) — so prefer the disable→enable replay above for debug loops.
- The VM exposes **no ICMP** — don't `ping` to check liveness. `nc -z <ip> 22` gives false
  negatives here; probe SSH with a banner grab instead
  (`timeout 3 bash -c 'exec 3<>/dev/tcp/<ip>/22 && head -c8 <&3' | grep SSH`) or just retry
  `win_exec`.
- A bad driver can wedge boot (e.g. GpuMmu + an *aperture* PageTableSegment hangs the display
  miniport early in PnP → SSH never comes up though QMP says `running`). It's a throwaway dev
  VM, but recover via the gpu-gl-out boot (below) to get SSH back and swap the `.sys`.

## 5. The gpu-gl / ntoseye dance (recovery + KD attach)

The Helios PCI device is the QEMU `virtio-gpu-gl-pci` device, id **`ua-heliosgpu`** (on `pci.8`,
`venus=true blob=true hostmem=8589934592` = 8 GiB host-visible BAR window). Owner-driven
procedure (the user runs it):

1. Bring the VM up **without gpu-gl** → Helios isn't enumerated → no bring-up, no
   hang/BSOD → clean autologin + SSH. (This is the recovery path after a wedging/BSODing build.)
2. With Helios absent, the live `.sys` is unlocked → deploy a known-good or new build freely.
3. Re-add the gpu-gl device → Helios enumerates → bring-up runs.
4. The user enables the **ntoseye MCP** (KD over `/tmp/ntoseye-kd.sock`).

When changing the VM launch / device set, STOP and let the user drive it (CLAUDE.md guardrail).

## 6. Reading driver state without a debugger

- **Diag ring = registry values** `S0..Sn` (REG_DWORD) under
  `HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`, written by `diag::record` (PASSIVE
  only — `RtlWriteRegistryValue`). Read over SSH; clear them with `Remove-ItemProperty` before a
  fresh run. `STEP` resets per driver load, so S0.. is the latest bring-up. Current code map:
  - `0x0100_00<type>` QueryAdapterInfo entry by type (`0D`=GPUMMUCAPS, `0E`=PAGETABLELEVELDESC,
    `0B`=QUERYSEGMENT4, `01`=DRIVERCAPS, `18/19`=perf-polls, gated out)
  - `0x0200_00<type>` QueryAdapterInfo answered NOT_SUPPORTED
  - `0x0300_00<ord>` GetNodeMetadata · `0x0600_0000` CreateProcess
  - `0x0800_0001/2` CreateContext / DestroyContext
  - `0x0900_0001` BAR-backed memory segment reported · `0x0900_0000` aperture fallback ·
    `0x0900_0002`/`0x0900_0003` = real-RAM paging segment present / absent (2-segment shape)
  - `0x0A00_0001/2` AddDevice entry/exit · `0x0A00_0003` paging-RAM contiguous alloc OK
  - `0x0B00_000x` StartDevice · `0x0B00_0006`/`0x0B00_00E6` ISR-status register mapped / not found
  - `0x0C01_xxxx` CreateAllocation
  - `0x0E00_0001` DestroyDevice entry, then `0x0F01..0x0F05` = DISPATCH-safe page-table atomics
    (BuildPagingBuffer mask/count, SetRootPageTable count, GetRootPageTableSize count, last op),
    and `0x0F06..0x0F0E` = engine atomics (SubmitCommand count, last fence, paging-submit count,
    Render, Patch, Preempt, InterruptRoutine, DpcRoutine, ControlInterrupt counts). All `0F`
    counts 0 ⇒ VidMm/VidSch never reached the page-table or engine DDIs (the adapter is torn down
    inside `DXGPROCESS_RENDER_ADAPTER_INFO::Initialize`, before them — see NTOSEYE.md / the memory).
- **DISPATCH-level DDIs cannot use `diag::record`** (RtlWriteRegistryValue is PASSIVE-only) —
  they use lock-free atomics in `build_paging_buffer.rs`, surfaced into the ring from the PASSIVE
  `DxgkDdiDestroyDevice` (see `diag_dump_gpummu_atomics`), or read by symbol under the KD.
- **Device status / Code:** `Get-PnpDevice -InstanceId …VEN_1AF4&DEV_1050…` →
  `Status` (`OK`/`Error`) and `Get-PnpDeviceProperty DEVPKEY_Device_ProblemCode`
  (`43` = FAILED_POST_START). `DEVPKEY_Device_ProblemStatus` (the NTSTATUS) reads **0/useless**
  here — dxgkrnl doesn't surface the VidMm reject NTSTATUS that way, and there's no dxgkrnl
  event-log entry either (it fails silently). Use the KD (NTOSEYE.md) for the reason.
- The Looking Glass IDD (`ROOT\DISPLAY\0000`) co-exists as a display adapter; VRD-pairing means
  DWM composites on Helios the instant it's Code 0, so a half-working Code-0 Helios crash-loops
  DWM/LogonUI (no soft launch). Do bring-up debugging with Helios at Code 43 / over the harness.

## 6b. New lessons (2026-06-22 composition bring-up)

- **ANY Rust panic in a Helios DDI = a silent graphics deadlock, not a clean error.** The no_std
  `#[panic_handler]` is `loop {}` (compiles to `eb fe` = `jmp $`). A panic spins that thread
  forever, and if it happened to hold dxgkrnl's adapter `ERESOURCE` (every DDI does, under
  `AcquireDdiSync`), the whole graphics stack deadlocks behind it (watchdog fires
  `DbgkWerCaptureLiveKernelDump`, "Possible deadlock"). Keep DDI paths panic-free: no unchecked
  indexing, no `unwrap`, mind debug-build overflow checks. The bug that bit us: `diag.rs` indexed
  a `[0u8; 3]` digit buffer with a 4-digit breadcrumb index once `STEP` passed 1000
  (`MAX_STEPS=3000`). To find a panic site live: a CPU stuck at `eb fe` in the driver is the
  handler; the caller's `panic_bounds_check(index,len,&Location)` regs + the `&Location`'s
  `src\file.rs` + line give the exact spot (read it from guest memory — SSH/`.map` aren't needed).
- **Deploy the IDD (LGIdd) with `devcon`, NOT in-place DriverStore copy.** The
  `lgidd.inf_amd64_*` DriverStore dir is TrustedInstaller-protected such that a copy can silently
  yield a 0-byte DLL (rename succeeds, the write doesn't) → the IDD fails to load and the device
  falls back to a stale `oemNN` copy. Use `devcon update <pkg>\LGIdd.inf "Root\LGIdd"` (installs a
  fresh DriverStore copy + rebinds) or `devcon restart "Root\LGIdd"` to re-run init without a new
  build. There are many stale `lgidd.inf_amd64_*` dirs; the active one is whichever
  `C:\Windows\INF\oemNN.inf` (from the device's `DEVPKEY_Device_DriverInfPath`) hashes to. Apply
  the same rule to Helios KMD: use `Z:\tools\install-helios-kmd.ps1` so package binding goes
  through `devcon update`/PnP, not manual DriverStore mutation.
- **The `win_looking_glass_idd` build prints an `InfVerif.dll`-missing error — it is non-fatal**;
  the signed `LGIdd.dll` + `lgidd.cat` are still produced.
- **Reading display state over SSH:** `QueryDisplayConfig` via `GetDisplayConfigBufferSizes`
  (flag 0x2 = active, 0x1 = all). Historical 2026-06-22 runs showed the path collapsing with
  Helios present while Helios-absent IDD activation worked; the 2026-06-23 same-boot check saw
  zero CCD paths even with Helios disabled, but a clean gpu-gl-out boot verified the baseline:
  WMI sees the LG monitor active, `Win32_VideoController` reports the Looking Glass IDD at
  `1920x1080`, and a session-1 CCD probe reports active/all/database paths. Use WMI or a
  scheduled `/IT` session-1 probe for monitor/CCD state; SSH/session 0 can be misleading. DWM
  crashes log to the Application event log (`Application Error` id 1000 + `Dwminit` id 0 with
  the exit HRESULT and "Primary display device ID").

## 6c. New lessons (2026-06-23 IDD + Helios composition bring-up)

- **Do display/monitor checks from session 1 or WMI, not an SSH/session-0 process.** SSH runs in
  session 0 and can report misleading desktop state. Reliable checks used this session:
  `Win32_VideoController`, `Win32_PnPEntity`, `root\wmi:WmiMonitorID`, and scheduled `/IT`
  helper/probe tasks in the active console session.
- **Current D3D11 state:** `D3D11CreateDevice` on Helios returns `S_OK` (`featureLevel=0xa000`).
  If this regresses, debug UMD/caps again; otherwise do not chase the old
  `dwmcore!CD3DDevice::CreateD3D11Device` / `0x889800b0` path.
- **Current Helios-present IDD/CCD symptom:** IddCx monitor arrival succeeds and
  `DisplayConfigGetDeviceInfo` can resolve the LG target, but
  `GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS)`, `QDC_ALL_PATHS`, and
  `QDC_DATABASE_CURRENT` all return `paths=0 modes=0`. The helper's supplied
  `SetDisplayConfig` path returns `ERROR_GEN_FAILURE` (`31`) and `EnumDisplaySettings` on the
  LG display returns `ERROR_BUSY` (`170`).
- **Clean gpu-gl-out baseline:** with Helios absent from the render/display path at boot
  (`Get-PnpDevice Status=Unknown`, `Problem=CM_PROB_PHANTOM`; earlier probe surfaced this as
  disconnected), `LGIddHelper` runs, the Looking Glass IDD reports `OK`, `WmiMonitorID`
  reports `DISPLAY\LGD1DDD...` active, `Win32_VideoController` reports `1920x1080`, and a
  session-1 `display_config_probe.exe` run returns `active paths=1 modes=2`,
  `all paths=2 modes=4`, `database paths=1 modes=2`. The IDD log shows OS-selected render
  adapter LUID
  `00000000:000076b0`, D3D11 feature level `0xb100`, D3D12 IVSHMEM heap/queues created, and
  the Looking Glass client displays the desktop.
- **IddCx WPP tracing works, decode still needs WPP format strings.** Capture:
  ```powershell
  logman create trace IddCx -o C:\Windows\Temp\IddCx-helios.etl -ets -ow -mode sequential -p {D92BCB52-FA78-406F-A9A5-2037509FADEA} 0x4f4 0xFF
  # cycle Root\LGIdd / trigger activation
  logman stop IddCx -ets
  ```
  `tracerpt` emits the ETL as CSV but leaves WPP events as `Unknown(...)`. Use
  `tracefmt.exe`/`tracepdb.exe` with public `IddCx.pdb`, or kernel `!wmitrace.logdump IddCx`.
- **IDD source restored.** No current diff remains under `LookingGlass/idd`. Earlier attempts
  to change monitor-mode `vSyncFreqDivider` broke `IddCxMonitorArrival`; do not reapply that.
- **KMD/UMD/ICD hot deploy can be reboot-free for these tests, but only through the verified
  scripts.** Do not add `COPYFLG_IN_USE_TRY_RENAME` to the `DIRID 13` copy line; WDK `infverif`
  rejects that combination. Default to `Z:\tools\install-helios-kmd.ps1`,
  `Z:\tools\hotplug-helios-umd.ps1`, and `Z:\tools\install-helios-icd.ps1`. Use
  `hotplug-helios-umd.ps1 -Mode PackageUpgrade` only when explicitly testing the
  Microsoft-shaped path: install a new `DIRID 13` package and rebind/restart so the UMD is loaded
  from a new unique DriverStore directory. Still use a full VM reset if the graphics stack wedges.
- **KMD resource lifetime fix from 2026-06-23:** standard WDDM allocations that adopt a
  KMD-created Venus `res_id` now remove the temporary owner-0 blob-table entry without host
  commands, so allocation destroy owns the detach/unref. This targets host log noise like
  `virgl_cmd_resource_unref: resource does not exist` / `ctrl 0x102 error 0x1203`.

## 7. Leave the VM clean

When pausing: remove any temp debug code (spin-gates, int3), rebuild (§1 purge!), repackage +
sign (§2), deploy (§3), and confirm the live `.sys` is `sig=Valid`. A throwaway dev VM tolerates
crashes, but the next session should find a clean, signed, loadable driver.
