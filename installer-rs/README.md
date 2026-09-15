# Helios vGPU Setup — self-contained installer (Rust)

`HeliosSetup.exe` is the user-facing installer for the Helios Windows graphics
stack, and it is a **single self-contained executable**: the whole bundle
(scripts, KMD, both UMDs, Mesa, CLVK, Khronos loaders, certificate, manifest) is
appended to its own PE image. There is no folder of loose files to keep together.

Rust owns the window, the container, process supervision and the CLI. The
install logic stays in the one PowerShell payload, which is embedded and
extracted at run time and executed by the in-box Windows PowerShell — the same
payload WinBoat's `-Automatic` flow and a human running it by hand use. There is
deliberately no second implementation of the ~30 install checks to drift.

## Usage

```
HeliosSetup.exe                          # GUI: Install / Repair / Update / Uninstall
HeliosSetup.exe --silent [--automatic]   # no UI; install or repair/update
HeliosSetup.exe --silent --uninstall     # no UI; remove
HeliosSetup.exe --silent --log PATH      # log to a chosen file
HeliosSetup.exe --bundle <payload> <out> # CI only: pack a payload folder into <out>
```

`--silent` writes `%ProgramData%\Helios\logs\setup.log` (or `--log`), returns the
payload's exit code (`0` success, `3010` reboot required, `2` uninstall-only
stored installer), and prints the required external step to the caller's console.

`--automatic` passes `-Automatic` to the payload: WinBoat's unattended flow, which
copies the bundle to `%ProgramData%\Helios\provisioning`, registers the resume
task, and writes `finished` on the post-reboot run. This is what the OEM
`install.bat` calls.

## External steps and orchestration

Installing a display driver needs a reboot, and enabling test-signing needs one
before that. Three channels report it:

1. **Exit code** — `0` / `3010` / non-zero.
2. **UI/console** — the GUI prompts "restart now?" and shows a Reboot button;
   `--silent` prints the step.
3. **`%ProgramData%\Helios\provisioning-status.json`** — written on every path:
   `waiting` → `test-signing-restart-required` → `driver-restart-required` →
   `finished`, or `failed` + message. This is the contract WinBoat polls.

## The container format

`src/archive.rs` owns both packing and unpacking:

```
[ PE image ][ entry data... ][ index ][ footer(64B: offset, size, index_off, sha256, "HLIOSET1") ]
```

Entries are raw-DEFLATE compressed. Extraction rejects absolute and
parent-traversing entry names. The whole container is SHA-256 checked before any
byte is written.

## Build

```
ci/windows/Build-Installer.ps1 -RepoRoot <repo> -OutputDir <out> -Configuration Release|Debug
```

The script builds with `cargo` (`+crt-static`), then asserts the result imports no
dynamic CRT and is a GUI-subsystem image. `build.rs` compiles `installer.rc`
(icon + manifest) with the Windows SDK `rc.exe`; the manifest requests
`requireAdministrator`, because the payload refuses to run unelevated.

The final self-contained exe is produced by the packaging step
(`ci/windows/Assemble-Package.ps1`), which calls
`HeliosSetup.exe --bundle <payloadDir> <out>` after assembling the payload.

## Files

| Path | Purpose |
|---|---|
| `src/main.rs` | CLI, payload extraction/resolution, process supervisor. |
| `src/gui.rs` | Win32/GDI window, painting, event loop (GDI only: runs on Microsoft Basic Display). |
| `src/archive.rs` | The self-contained container (pack + unpack). |
| `src/installer.manifest` | Common Controls v6, DPI, `requireAdministrator`. |
| `installer.rc`, `build.rs` | Icon + manifest embedding via `rc.exe`. |
| `assets/` | `logo_bg.bmp` (logo composited on the window background), `helios.ico`, `winboat_logo.svg` source. |

The logo BMP is regenerated from the SVG with
`magick winboat_logo.svg -background '#14161B' -flatten -resize 128x128 -type TrueColor bmp3:logo_bg.bmp`.
