# KMD/UMD quality refactor handoff

The next workstream is a behavior-preserving quality refactor of
`kmd_render` and `umd`. It must be performed in three ordered phases:

1. Review `kmd_render` and `umd` adversarially and in parallel. Do not edit
   production code during this phase. Write one review document containing
   dependency-ordered, preferably atomic recommendations with concrete file,
   symbol, responsibility, risk, and validation references. Static guarantees
   are a primary review axis, not optional cleanup.
2. Implement the accepted recommendations in small commits. Split oversized
   files along real responsibility boundaries, remove redundant or obsolete
   paths, make invariants explicit in types/guards, and consolidate repeated
   behavior. Avoid semantic rewrites disguised as file moves.
3. Build, deploy, and regression-test the driver. Visible desktop output is the
   success criterion; a log-only success is not sufficient.

## Frozen baseline

- KMD `22.22.142.0` is active.
- DWM renders directly into the exact Windows-designated OPTIMAL primary; there
  is no guest primary-to-scanout copy.
- KMD refresh markers capture a Venus wire-fence watermark under the
  `WddmNotifyGuard` lock-order proof and are consumed by the used-ring DPC.
- The UMD closes the DXVK submission-thread gap with a bounded, condition-
  variable-backed 10 ms frame-completion gate. This is not a polling `Sleep`
  loop: normal completion wakes immediately and measured steady average was
  about 0.48 ms.
- QEMU reconstructs the modifier-less OPTIMAL image and reads it back for the
  display frontend without changing the virtio-gpu protocol ABI.
- SDL OpenGL on native Wayland and egl-headless plus VNC have visible-output
  verification. GTK/Wayland remains blocked by GDK `eglMakeCurrent` failures.
- The owner confirmed excellent responsiveness and no fast-cursor ghosting.
- `ScanoutDiag` is absent and must remain absent during primary tests.

## Review targets

The review should prioritize:

- files with multiple unrelated responsibilities or excessive length;
- duplicated display, present, allocation, synchronization, and diagnostic
  logic;
- legacy IddCx/vehicle/copy paths that are still compiled into active flows;
- heuristics, magic values, side-effect-based ordering, and contracts enforced
  only by comments;
- synchronous control roundtrips, polling, `Sleep`, and timeout loops that can
  be replaced by interrupts, events, fences, or condition variables;
- error paths that report success after partial failure;
- unsafe ownership/lifetime assumptions and lock ordering not witnessed by
  types or guards;
- telemetry that performs I/O or expensive formatting on hot paths.

Do not mechanically remove every timeout. A bounded timeout around a real
event/fence wait is a safety contract; an arbitrary delay used to make ordering
appear correct is a hack. Review findings must distinguish the two.

## Static-guarantee requirement

For every important invariant currently enforced by a comment, magic value,
nullable raw pointer, boolean combination, call order, side effect, or runtime
assertion, the review must explicitly ask whether the compiler can enforce it.
Each applicable finding must document:

1. the current runtime-only invariant and the invalid state/call sequence it
   permits;
2. the proposed compile-time representation;
3. the smallest atomic migration boundary;
4. any remaining `unsafe` preconditions and why they cannot be encoded;
5. the regression test which proves behavior was preserved.

High-value patterns to examine include:

- newtypes for resource IDs, allocation handles, wire fences, scanout
  dimensions/strides, and distinct guest/host or primary/fallback identities;
- non-null and lifetime-bearing wrappers instead of repeatedly reinterpreting
  raw WDDM handles and COM pointers;
- typestate for adapter/device/context lifecycle, allocation construction,
  scanout publication, and asynchronous submit/retire ownership;
- enums with exhaustive matching instead of interdependent booleans, sentinel
  integers, and undocumented diagnostic status codes;
- RAII guards for locks, IRQL-sensitive regions, mapped memory, COM ownership,
  kernel objects, and in-flight DMA/control buffers;
- proof tokens like `WddmNotifyGuard` wherever lock ordering, execution level,
  or notification authority is part of the contract;
- APIs which consume ownership when an object becomes queued/in-flight and
  return a distinct completed object on retirement, preventing reuse while the
  host or GPU still owns it;
- constructors which validate format, extent, pitch, offset, memory size, and
  exportability once, yielding a validated scanout descriptor that downstream
  code cannot partially initialize;
- sealed interfaces which prevent fallback/diagnostic resources from entering
  the exact-primary path;
- compile-time separation of passive-level, dispatch-level, and DPC-only
  operations where Rust's types can carry the proof.

Do not introduce wrapper types which merely relocate unchecked casts or make
`unsafe` broader. The review should favor a small trusted boundary with safe,
restrictive APIs, and should call out cases where a proposed static guarantee
would be cosmetic rather than real.

## Regression gate

At minimum, every implementation tranche must pass:

- KMD and release UMD builds plus formatting/diff checks;
- healthy Helios device state and expected driver/UMD binding;
- `ScanoutDiag` absent, `VpSA=1`, and `ScSet=1` on the current activation;
- visible desktop, idle-to-active responsiveness, rapid cursor motion without
  trails, and no unprompted DWM crash;
- no new present-gate steady-state timeouts, control timeouts, or ring failures;
- DComp present cadence near the established 63 fps baseline;
- same-boot QEMU evidence for the actual OPTIMAL DWM primary, not a diagnostic
  fill image.

Guest reboots are disruptive and should be requested before use. Adapter
restarts are sufficient for UMD-only deployments; a newly built KMD image
requires a guest reboot.
