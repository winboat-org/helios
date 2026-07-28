//! The UMD's per-process log file and the two macros that write to it.
//!
//! Moved verbatim out of `lib.rs` by T8/R1106. `lib.rs` re-exports `log_line`,
//! `trace_line!` and `log_error!` so the ~2000 call sites across `forward.rs`,
//! `device_funcs.rs` and `bridge.rs` -- and the macro bodies' own
//! `crate::log_line` paths -- are untouched.

use core::ffi::c_void;
use std::io::Write;

use crate::knobs;

/// Resolve the per-process UMD log path, computed once.
///
/// The restricted IddCx host process (which opens the IDD swapchain surface)
/// cannot write `C:\Windows\Temp\helios_umd.log` — that directory's ACL only
/// grants SYSTEM/Administrators, so the IDD process's log lines vanished. We log
/// to a per-pid file under `C:\ProgramData\Helios\` instead: standard users may
/// create files there (inherited ProgramData ACL), and a per-pid name means each
/// process owns its own file with full control regardless of who created the dir.
pub(crate) fn umd_log_path() -> &'static std::path::Path {
    use std::sync::OnceLock;
    static PATH: OnceLock<std::path::PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let dir = std::path::Path::new(r"C:\ProgramData\Helios");
        // Best effort: ignore AlreadyExists / permission errors.
        let _ = std::fs::create_dir_all(dir);
        dir.join(format!("umd-{}.log", std::process::id()))
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
/// - [`trace_line!`] — per-op repeat traffic. `UmdTrace`-gated, and it does not
///   even evaluate its arguments when the knob is off.
#[deprecated(note = "use log_error! (errors/one-shots/refusals) or trace_line! (per-op traffic)")]
pub(crate) fn log_line(message: &str) {
    if let Ok(mut slot) = log_file().lock() {
        if let Some(f) = slot.as_mut() {
            let _ = writeln!(f, "[pid={}] {}", std::process::id(), message);
        }
    }
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
/// Called only from `DllMain(DLL_PROCESS_DETACH)` with `lpReserved == NULL`,
/// i.e. the `FreeLibrary` case. On process teardown the kernel reclaims
/// everything and touching a lock there buys nothing.
///
/// `try_lock`, not `lock`: DllMain runs under the loader lock, and blocking on
/// a mutex another thread holds is the textbook loader deadlock. A thread
/// inside `log_line` while its own module is being unloaded is already a
/// use-after-free hazard the loader created, so the contended case is not
/// reachable in any healthy teardown — it is counted rather than waited on.
pub(crate) fn close_at_detach() {
    use std::sync::atomic::Ordering;
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
pub(crate) static LOG_CLOSE_CONTENDED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// The log mutex if it has already been created, WITHOUT creating it.
///
/// `close_at_detach` must not be the call that first opens the log file: a
/// process that never logged has no handle to release, and `OnceLock::get`
/// answers "already initialised?" without initialising, where `get_or_init`
/// would strand the very handle this exists to close.
fn log_file_if_open() -> Option<&'static std::sync::Mutex<Option<std::fs::File>>> {
    LOG_FILE.get()
}

static LOG_FILE: std::sync::OnceLock<std::sync::Mutex<Option<std::fs::File>>> =
    std::sync::OnceLock::new();

/// Per-frame/per-op trace logging, gated by [`trace_enabled`]. The format
/// arguments are not evaluated when tracing is off.
macro_rules! trace_line {
    ($($arg:tt)*) => {
        if crate::trace_enabled() {
            #[allow(deprecated)]
            crate::log_line(&format!($($arg)*));
        }
    };
}
pub(crate) use trace_line;

/// Unconditional log line: errors, one-shots and refusals ONLY.
///
/// The counterpart to [`trace_line!`]. Per-op repeat traffic must not use this
/// — that is what put a 21-argument `format!` plus a mutex-guarded unbuffered
/// write on all seven draw entry points and on the caps-query path (R420).
macro_rules! log_error {
    ($($arg:tt)*) => {{
        #[allow(deprecated)]
        crate::log_line(&format!($($arg)*));
    }};
}
pub(crate) use log_error;

/// Log which DLL file THIS code is running from, once per process. Multiple
/// UMD copies exist on disk (DriverStore package, ProgramData versioned
/// copies) and boot-time resolution has served stale builds before (a stray
/// pre-typed-signature FileRepository\helios_umd.dll caused cold-boot dwm
/// devices to run old shader handlers, 2026-07-04) — the per-pid log alone
/// cannot distinguish which copy handled which device.
pub(crate) fn log_self_module_path() {
    use std::sync::atomic::{AtomicBool, Ordering};
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
                log_error!("UMD module: {}", String::from_utf16_lossy(&buf[..n]));
                return;
            }
        }
        log_error!("UMD module: <unresolvable>");
    }
}

/// Log every registry knob and its resolved value, once per process.
///
/// The reader that makes [`crate::knobs`]'s inventory more than a comment: it
/// turns "which knobs were in force in this process" from a re-derivation into
/// a fact in the log, next to the module path that says which DLL produced it.
/// It is also R1008's own validation instrument — the defaults moved from four
/// hand-written tail expressions into constructor arguments, and this line is
/// what proves the resolved values did not move with them.
///
/// Resolving forces all four `OnceLock`s here rather than at each knob's first
/// use. Every one is read once per process either way, and the documented A/B
/// procedure is "write the value, then start a new process (or
/// `pnputil /restart-device`)" — which loads the DLL after the write in both
/// orderings, so the resolved values are the same.
pub(crate) fn log_knob_inventory() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if LOGGED.swap(true, Ordering::Relaxed) {
        return;
    }
    for (name, value) in knobs::resolved_inventory() {
        log_error!("UMD knob: {name}={value}");
    }
}
