//! The UMD's per-process log file and the two macros that write to it.
//!
//! Moved from `umd/src/log.rs` (`DECISIONS.md` D3b, stage S2). It arrived there
//! from `lib.rs` by T8/R1106; this is the second relocation and, like the first,
//! every call site is untouched — `umd/src/lib.rs` re-exports `log_error`,
//! `trace_line`, `log_line`, `log_self_module_path` and `log_knob_inventory` at
//! the crate root, so the ~430 macro uses across `forward/*`, `device_funcs.rs`
//! and `adapter.rs` still name `crate::…`.
//!
//! # The one thing that is genuinely new: [`init`]
//!
//! D3D11 keeps `umd-<pid>.log` and D3D12 gets `umd12-<pid>.log`. ⛔ **Two
//! drivers appending to one file would interleave unreadably** and would break
//! the evidence discipline that reads them per module — the id-1000 check, the
//! knob inventory, `tools/capture-knob-inventory.ps1`. The C++ bridges draw the
//! same line: `bridge_common.h` declares `umd_log` and each bridge defines it.
//!
//! ⚠ The basename defaults to `"umd"`, deliberately. That makes the D3D11 path
//! **provably unchanged** by this move — which is S2's pass criterion — and puts
//! the burden of calling [`init`] early on the crate that wants a different
//! name. A late or missing `init` is not silent: see [`LOG_INIT_LATE`].
//!
//! # R420's guarantee, preserved across two crates
//!
//! [`log_line`] is `#[deprecated]` purely as an internal marker, and `umd`'s
//! `#![deny(deprecated)]` turns a direct call into a compile ERROR. Deprecation
//! is cross-crate, so the guarantee survives the move: only [`trace_line!`] and
//! [`log_error!`] — each wrapping the call in `#[allow(deprecated)]` at the
//! expansion site — may reach the unconditional writer. Verified by fault
//! injection each time this moves.

use core::ffi::c_void;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;

/// The log basename, set by [`init`]. `"umd"` when nothing set it.
static BASENAME: OnceLock<&'static str> = OnceLock::new();

/// The resolved log path. Latched by the first [`umd_log_path`] call, which is
/// the first log line of any kind — [`init`] reads it to detect being too late.
static LOG_PATH: OnceLock<std::path::PathBuf> = OnceLock::new();

/// Whether per-op trace traffic is enabled, resolved once by [`init`].
///
/// ⚠ A plain relaxed `AtomicBool` load, NOT a call through a registered
/// `fn() -> bool`. `trace_line!` expands at ~430 sites, many of them per-op, and
/// an indirect call on every one of them is a perf change — which in stage S2 is
/// a defect, not an improvement. The knob VALUE still lives in the calling
/// crate (D3b: "the knob values stay per-crate"); only its resolved answer is
/// cached here.
static TRACE: AtomicBool = AtomicBool::new(false);

/// [`init`] calls that arrived after the log path was already latched, with a
/// DIFFERENT basename — i.e. something logged before the driver named itself
/// and the lines went to the wrong module's file.
///
/// CLAUDE.md rule 2: every skipped path gets a named counter. Expected 0. A
/// non-zero value here (and the `log_error!` on the first hit) is what stops
/// `umd12` silently appending to `umd-<pid>.log` forever.
pub static LOG_INIT_LATE: AtomicUsize = AtomicUsize::new(0);

/// Name this module's log file and resolve its trace gate. Call it **before the
/// first line this driver logs** — for `umd` that is the top of
/// `open_adapter_common`, which is the first entry point the runtime calls.
///
/// `basename` becomes `C:\ProgramData\Helios\<basename>-<pid>.log`.
/// `trace` is the caller's own resolved `UmdTrace`-equivalent knob.
///
/// Idempotent, and loud if it is too late to matter.
pub fn init(basename: &'static str, trace: bool) {
    TRACE.store(trace, Ordering::Relaxed);
    let _ = BASENAME.set(basename);

    // ⚠ The check is on PATH, not on BASENAME. Setting the name late is
    // harmless if nothing has logged yet; what is NOT recoverable is the log
    // path having already been latched by an earlier line, because `PATH` is a
    // `OnceLock` and the file handle behind it is process-lifetime. A
    // `BASENAME.set` failure alone would also fire on the perfectly normal
    // second `OpenAdapter` in one process, which is not a defect.
    if let Some(path) = LOG_PATH.get() {
        let want = format!("{basename}-{}.log", std::process::id());
        if path.file_name().and_then(|s| s.to_str()) != Some(want.as_str())
            && LOG_INIT_LATE.fetch_add(1, Ordering::Relaxed) == 0
        {
            crate::log_error!(
                "UMD log init LATE: wanted {want}, but {} was already latched by an earlier \
                 log line - this module's lines are in another module's file",
                path.display()
            );
        }
    }
}

/// Whether per-op/per-frame trace traffic ([`trace_line!`]) is enabled.
///
/// Answers `false` until [`init`] runs. That is the intended reading — a driver
/// that has not opened its adapter yet has no per-op traffic to trace — and it
/// is why `init` goes at the TOP of the adapter open, above the first
/// `log_error!`.
#[inline]
pub fn trace_enabled() -> bool {
    TRACE.load(Ordering::Relaxed)
}

/// Resolve the per-process UMD log path, computed once.
///
/// The restricted IddCx host process (which opens the IDD swapchain surface)
/// cannot write `C:\Windows\Temp\helios_umd.log` — that directory's ACL only
/// grants SYSTEM/Administrators, so the IDD process's log lines vanished. We log
/// to a per-pid file under `C:\ProgramData\Helios\` instead: standard users may
/// create files there (inherited ProgramData ACL), and a per-pid name means each
/// process owns its own file with full control regardless of who created the dir.
///
/// ⚠ The file is APPEND-ONLY and PIDs are reused across boots, so one file can
/// hold several driver generations. Anything reading it for evidence must take
/// the block after the last `UMD module:` line — `tools/capture-knob-inventory.ps1`
/// exists because that is not obvious.
pub fn umd_log_path() -> &'static std::path::Path {
    LOG_PATH.get_or_init(|| {
        let dir = std::path::Path::new(r"C:\ProgramData\Helios");
        // Best effort: ignore AlreadyExists / permission errors.
        let _ = std::fs::create_dir_all(dir);
        dir.join(format!(
            "{}-{}.log",
            BASENAME.get().copied().unwrap_or("umd"),
            std::process::id()
        ))
    })
}

/// The unconditional log writer.
///
/// DO NOT CALL DIRECTLY — it is `#[deprecated]` purely as an internal marker so
/// the compiler enforces that. Use one of the two macros, which is the whole
/// point of R420's static guarantee: the choice between "this is an error, a
/// one-shot or a refusal" and "this is per-op repeat traffic" has to be made
/// explicitly at every site, and a new per-op site cannot reach the
/// unconditional writer by accident.
///
/// - [`log_error!`] — errors, one-shots, refusals. Always written.
/// - [`trace_line!`] — per-op repeat traffic. Trace-gated, and it does not
///   even evaluate its arguments when the gate is off.
#[deprecated(note = "use log_error! (errors/one-shots/refusals) or trace_line! (per-op traffic)")]
pub fn log_line(message: &str) {
    if let Ok(mut slot) = log_file().lock() {
        if let Some(f) = slot.as_mut() {
            // tid in the prefix: once creates/destroys go free-threaded and
            // deferred contexts record on worker threads, a pid-only prefix
            // makes concurrent DDI traffic unattributable.
            let _ = writeln!(
                f,
                "[pid={} tid={}] {}",
                std::process::id(),
                current_thread_id(),
                message
            );
        }
    }
}

/// Win32 thread id of the calling thread (`GetCurrentThreadId`). Cheap: reads
/// the TEB, no syscall.
fn current_thread_id() -> u32 {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }
    // SAFETY: no arguments, no pointers; always valid to call.
    unsafe { GetCurrentThreadId() }
}

/// The process-lifetime log handle.
///
/// One handle per DLL instance: the old open/append/close-per-line pattern cost
/// a full CreateFile round trip on every logged DDI call — measurable on
/// per-frame paths (PSC WS2). Unbuffered File writes keep crash durability.
///
/// The payload is an `Option` so [`close_at_detach`] can take the `File` and
/// let its `Drop` close the handle. It is NOT merely `Option` for open
/// failure: a `OnceLock` is never dropped, so with a plain `Mutex<File>` there
/// is no way to release the handle at all.
fn log_file() -> &'static std::sync::Mutex<Option<std::fs::File>> {
    LOG_FILE.get_or_init(|| {
        std::sync::Mutex::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(umd_log_path())
                .ok(),
        )
    })
}

/// Close the log handle because this DLL is being unloaded.
///
/// `helios_umd.dll` is loaded and unloaded ONCE PER D3D11 DEVICE — measured
/// directly (`GetModuleHandleW` reads NO / yes / NO across one
/// `D3D11CreateDevice` + `Release` pair, and this file's own once-per-process
/// `UMD module:` line appears once per device in the log). Neither a Rust
/// `static` nor a `OnceLock` payload is ever dropped, and the loader does not
/// close handles a module opened, so the log handle above was stranded on
/// every unload: exactly one leaked kernel `File` per device, forever, with no
/// plateau. That was one of the six handles per device
/// `tools/helios_ownership_soak.cpp` has reported since T5; the other five
/// belong to the venus ICD.
///
/// ⚠ Whether `helios_umd12.dll` is ALSO loaded/unloaded once per device is
/// UNVERIFIED-5 and is scheduled for S5. Every never-freed process-lifetime
/// handle in a second UMD would double the leak, which is why this function is
/// shared rather than reimplemented.
///
/// Called only from `DllMain(DLL_PROCESS_DETACH)` with `lpReserved == NULL`,
/// i.e. the `FreeLibrary` case. On process teardown the kernel reclaims
/// everything and touching a lock there buys nothing.
///
/// `try_lock`, not `lock`: DllMain runs under the loader lock, and blocking on
/// a mutex another thread holds is the textbook loader deadlock. A thread
/// inside `log_line` while its own module is being unloaded is already a
/// use-after-free hazard the loader created, so the contended case is not
/// reachable in any healthy teardown — it is counted rather than waited on.
pub fn close_at_detach() {
    let Some(lock) = log_file_if_open() else {
        return;
    };
    match lock.try_lock() {
        // Dropping the `File` is what closes the handle. Subsequent
        // `log_line` calls find `None` and become no-ops, which is correct:
        // the module they would log from is going away.
        Ok(mut slot) => {
            *slot = None;
        }
        // Refused, not silently skipped. The handle stays open and leaks, so
        // the live signal is the soak's own per-device handle rate going back
        // above zero; this counter is what names the reason in a dump.
        Err(_) => {
            LOG_CLOSE_CONTENDED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Log-handle closes refused because another thread held the writer lock at
/// `DLL_PROCESS_DETACH`. Expected 0: see [`close_at_detach`].
pub static LOG_CLOSE_CONTENDED: AtomicUsize = AtomicUsize::new(0);

/// The log mutex if it has already been created, WITHOUT creating it.
///
/// `close_at_detach` must not be the call that first opens the log file: a
/// process that never logged has no handle to release, and `OnceLock::get`
/// answers "already initialised?" without initialising, where `get_or_init`
/// would strand the very handle this exists to close.
fn log_file_if_open() -> Option<&'static std::sync::Mutex<Option<std::fs::File>>> {
    LOG_FILE.get()
}

static LOG_FILE: OnceLock<std::sync::Mutex<Option<std::fs::File>>> = OnceLock::new();

/// Per-frame/per-op trace logging, gated by [`trace_enabled`]. The format
/// arguments are not evaluated when tracing is off.
///
/// ⚠ `$crate`-qualified throughout, so it resolves identically no matter which
/// cdylib expands it and regardless of what that crate calls its own modules.
#[macro_export]
macro_rules! trace_line {
    ($($arg:tt)*) => {
        if $crate::log::trace_enabled() {
            #[allow(deprecated)]
            $crate::log::log_line(&format!($($arg)*));
        }
    };
}

/// Unconditional log line: errors, one-shots and refusals ONLY.
///
/// The counterpart to [`trace_line!`]. Per-op repeat traffic must not use this
/// — that is what put a 21-argument `format!` plus a mutex-guarded unbuffered
/// write on all seven draw entry points and on the caps-query path (R420).
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {{
        #[allow(deprecated)]
        $crate::log::log_line(&format!($($arg)*));
    }};
}

/// Log which DLL file THIS code is running from, once per process. Multiple
/// UMD copies exist on disk (DriverStore package, ProgramData versioned
/// copies) and boot-time resolution has served stale builds before (a stray
/// pre-typed-signature FileRepository\helios_umd.dll caused cold-boot dwm
/// devices to run old shader handlers, 2026-07-04) — the per-pid log alone
/// cannot distinguish which copy handled which device.
///
/// ⚠ The anchor is this function's own address, so with two Helios UMDs in one
/// process each module's call reports **its own** file. That is the property
/// `tools/capture-knob-inventory.ps1` keys on, and it is why this is a shared
/// function rather than a shared string.
pub fn log_self_module_path() {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if LOGGED.swap(true, Ordering::Relaxed) {
        return;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleExW(flags: u32, module_name: *const u16, module: *mut *mut c_void)
            -> i32;
        fn GetModuleFileNameW(module: *mut c_void, filename: *mut u16, size: u32) -> u32;
    }
    const FROM_ADDRESS: u32 = 0x4; // GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
    const UNCHANGED_REFCOUNT: u32 = 0x2; // ..._UNCHANGED_REFCOUNT
    unsafe {
        let mut hmod: *mut c_void = core::ptr::null_mut();
        let anchor = log_self_module_path as *const ();
        if GetModuleHandleExW(
            FROM_ADDRESS | UNCHANGED_REFCOUNT,
            anchor as *const u16,
            &mut hmod,
        ) != 0
        {
            let mut buf = [0u16; 512];
            let n = GetModuleFileNameW(hmod, buf.as_mut_ptr(), buf.len() as u32) as usize;
            if n > 0 && n < buf.len() {
                crate::log_error!("UMD module: {}", String::from_utf16_lossy(&buf[..n]));
                return;
            }
        }
        crate::log_error!("UMD module: <unresolvable>");
    }
}

/// Log every registry knob and its resolved value, once per process.
///
/// The reader that makes the knob inventory more than a comment: it turns
/// "which knobs were in force in this process" from a re-derivation into a fact
/// in the log, next to the module path that says which DLL produced it. It is
/// also R1008's own validation instrument — the defaults moved from four
/// hand-written tail expressions into constructor arguments, and this line is
/// what proves the resolved values did not move with them. **It is now this
/// stage's instrument too**: `S2-check` requires its output to be byte-identical
/// before and after this very move.
///
/// ⚠ `entries` is a parameter because the KNOB SET is per-crate (D3b) — `umd12`
/// declares its own, including `UmdD3D12`. Resolving them forces every
/// `OnceLock`, which is why the caller passes an already-resolved array rather
/// than this function reaching for a knob table it cannot know about.
///
/// The caller is expected to be its crate's own thin wrapper, so that
/// `crate::log_knob_inventory()` keeps resolving at every existing call site.
pub fn log_knob_inventory(entries: &[(&'static str, u32)]) {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if LOGGED.swap(true, Ordering::Relaxed) {
        return;
    }
    for (name, value) in entries {
        crate::log_error!("UMD knob: {name}={value}");
    }
}
