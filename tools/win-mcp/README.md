# win-mcp — driving the Windows hosts without the usual pain

This directory is a stdio MCP server **and** a CLI. Both fronts use the same
implementation, so anything an MCP client can do, a shell script (or an agent
whose harness only gives it bash) can do too:

```sh
cargo build --release --offline            # CARGO_TARGET_DIR=target/linux
BIN=target/linux/release/win-mcp
$BIN --cli hosts
$BIN --cli status --host vm                # one-shot: what is actually running
$BIN --cli preflight --host slave          # toolchain/tree/disk/principal
```

Everything is written to remove a specific class of wasted time. The list below
is empirical — each item cost a real diagnostic cycle.

## The two hosts

| name | ssh target | workspace | staging | default purpose |
|---|---|---|---|---|
| `vm` | `win` (win11 dev VM) | `C:\Users\Tibix` | `C:\Users\Tibix\winrun` | `desktop` |
| `slave` | `firstheberg2-win` (build slave) | `C:\src` | `C:\src\winrun` | `build` |

Overridable per deployment: `HELIOS_SSH_VM`, `HELIOS_SSH_SLAVE`,
`HELIOS_SSH_CONFIG` (see below), `HELIOS_LINUX_PROJECT_ROOT` (where this repo is).

The domain tools (`win_exec`, `win_cargo`, `win_vkd3d`, `win_meson`,
`win_install_*`, …) remain VM- and `Z:\`-specific. The `win_host_*` tools are the
host-agnostic layer.

## `purpose` is part of correctness, not a detail

* **`desktop`** — anything that observes or drives the real desktop: D3D12
  probes, DWM state, benchmarks. Runs as the interactive user and **refuses
  session 0** (exit 87). A GPU probe started from session 0 does not fail; it
  returns a plausible, wrong answer, which is worse.
* **`build`** — compilation and packaging. Runs as SYSTEM, and additionally sets
  `safe.directory` (SYSTEM is not the tree's owner) and rewrites `PATH` so that
  `C:\msys64\usr\bin` cannot shadow Windows Git with the MSYS2 one, which reports
  a Windows-path repository as *"Not a git repository"*.
* **`install`** — driver/package installation. Runs as SYSTEM so a console
  control event from an interactive session cannot interrupt a display-driver
  swap (`STATUS_CONTROL_C_EXIT` at `pnputil /add-driver` is the recorded case).
* **`system`** — generic privileged work.

Long work (a driver build is ~8 minutes) must be detached: an ssh keepalive drop
kills a synchronous remote process. `run-script --task NAME` registers a
scheduled task, redirects to a log, and appends a `WINRUN_EXIT=<code>` marker;
`task status` reads state, `LastTaskResult`, the log tail and that marker.

## Commands

```
hosts                                   which host resolves to what (and the -F decision)
hostinfo  --host H                      identity, session, admin, interactive sessions, disk
preflight --host H                      tools/compilers/tree/disk/tasks — everything up front
status    --host H                      one-shot Helios stack status (JSON)
run       --host H --command '<ps>'     ad-hoc snippet under a purpose
run-script --host H <local.ps1>         verified push, then run (or --task NAME to detach)
task start|status|kill --host H --name N
push/pull --host H <a> <b>              sha256 verified on BOTH ends
emit      --dir <dir>                   write the shipped .ps1 payloads out to read
sha256    <local>                       the same digest function push uses
```

All commands exit non-zero on failure; a remote non-zero exit is propagated
(so a purpose refusal is distinguishable as 87). `--json` where a machine
readable form exists; `status`, `preflight` and `hostinfo` are JSON already.

## What is encoded here (and why)

* **`ssh -F ~/.ssh/config` auto-detection.** In a container whose
  `/etc/ssh/ssh_config.d` is not root-owned, OpenSSH 10.2 rejects the *system*
  config and exits 255 with one warning line and nothing else, so `ssh host`
  fails while `ssh -F ~/.ssh/config host` works. Detected once per process and
  printed by `--cli hosts`.
* **Base64/UTF-16LE `-EncodedCommand`** for every snippet: immune to bash → ssh
  → cmd/PowerShell quoting, and it propagates the real exit code.
* **One token per `-File` argument.** `powershell -File` does not split a comma
  list or a quoted `"flag value"` pair: `'-Role vm'` arrives as one argument and
  fails parameter binding. Pass `["-Role","vm"]`.
* **Every embedded path goes through one quoting helper.** `ps_single_quote`
  *escapes*; forgetting to also *wrap* silently broke a log path containing a
  space. `host::lit` escapes and wraps, and the unit tests pin both.
* **Verified transfers.** `scp` succeeding is not evidence that the bytes
  arrived; every push/pull hashes on both ends and compares size.
* **Idempotent `safe.directory`.** `git config --add` appends a duplicate per
  call (six builds, six entries); it is now check-then-add.
* **A JSON envelope** (`WINRUN_JSON_BEGIN/END`) around payload output, so a
  script's own progress lines can never corrupt the payload.
* **`emit`** exists so the PowerShell payloads can be read and diffed as files
  rather than extracted from Rust string literals.

## Verifying the layer

The unit tests cover the pure builders — quoting, argument splitting, the
session guard, principal selection, the JSON envelope, digests:

```sh
HELIOS_LINUX_PROJECT_ROOT=$PWD CARGO_TARGET_DIR=target/linux \
  cargo test --release --offline
```

⚠ `coreutils`-style tests in this crate read the real checkout, so
`HELIOS_LINUX_PROJECT_ROOT` is set above; without it one pre-existing test
(`kmd_version_bump_roundtrip`) fails on a checkout that is not the owner's.

End to end, the smoke script asserts what a purpose promises (PATH hygiene,
session id) and is the quickest way to prove a host is sane:

```sh
$BIN --cli run-script --host slave smoke.ps1 --task winrun-smoke-build --purpose build
$BIN --cli task status --host slave --name winrun-smoke-build
$BIN --cli run-script --host vm    smoke.ps1 --task winrun-smoke-desktop --purpose desktop
$BIN --cli task status --host vm    --name winrun-smoke-desktop
```

The desktop case must report `session_id: 1` and the build case must report a
`PATH` without `msys64\usr\bin`; both must end with `SMOKE PASS` and
`WINRUN_EXIT=0`.

## Adding a host

Add a `HostSpec` const in `src/host.rs` (ssh target env/default, workspace,
staging, PowerShell path, interactive user, build PATH prefix, default purpose)
and extend `host_spec`. Nothing else needs to change: the payload scripts are
already role-parameterised (`status.ps1 -Role …`).
