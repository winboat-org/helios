//! The refusal-counter **mechanism**: a named atomic that emits its whole set's
//! summary on its first hit.
//!
//! Moved from `umd/src/forward.rs:322-448` (`DECISIONS.md` D3b, stage S2) and
//! generalised, as D3b asks, from a bare `&AtomicUsize` to a
//! [`RefusalCounter`] that carries its own name.
//!
//! ⛔ **Only the mechanism moved.** The eleven D3D11 counters stay in `umd`;
//! `umd12` declares its own set. A shared counter list would make
//! `DDI refusals:` a line about two drivers, and `CONFORMANCE.md`'s charter —
//! *"drive the UMD's `DDI refusals:` counters to zero"* — reads it per driver.
//!
//! # Why the summary is emitted on the FIRST hit
//!
//! The UMD's evidence channel is the log; it has no registry counter surface.
//! T5 proved the failure mode this avoids: three of the four R806/R809 scan-out
//! counters were process-global atomics that NOTHING ever loaded, so ROADMAP's
//! own instruction to read them after a gate run was not executable. **An
//! instrument nothing can read is not an instrument.**
//!
//! Emitting on the first hit of each counter (in addition to a summary at
//! device teardown) means a refusal that fires once, in a session that never
//! tears a device down, is still visible.
//!
//! ⚠ NOT on a per-present path: the UMD hot-path logger cost is exactly what T2
//! measured and reduced. [`RefusalCounter::note`] is one relaxed
//! `fetch_add` in the steady state and formats nothing.

use core::sync::atomic::{AtomicUsize, Ordering};

/// One named refusal counter.
///
/// Carrying the name here rather than at the summary site is what lets a
/// summary be built by iterating a slice instead of by a hand-written format
/// string with one `{}` per field — the shape that made adding a twelfth
/// counter a three-place edit, with the third place (the format string) silently
/// optional.
pub struct RefusalCounter {
    count: AtomicUsize,
    name: &'static str,
}

impl RefusalCounter {
    pub const fn new(name: &'static str) -> Self {
        Self {
            count: AtomicUsize::new(0),
            name,
        }
    }

    /// The counter's name, as it appears in a summary line.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Current value.
    pub fn get(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    /// Bump the counter. Returns `true` on the **first** hit, which is the
    /// caller's cue to emit its set's summary.
    ///
    /// Returning a bool rather than emitting here keeps this module free of any
    /// knowledge of *which* set the counter belongs to, which is the whole
    /// reason the sets stay per-crate.
    #[must_use = "the first hit must emit the set's summary, or the counter is unreadable"]
    pub fn note(&self) -> bool {
        self.count.fetch_add(1, Ordering::Relaxed) == 0
    }

    /// Bump the counter **without** the first-hit signal, for the one shape
    /// where the refusing site already logs its own line.
    ///
    /// ⚠ Deliberately a second method rather than `let _ = note()`. R911 is
    /// explicit that an already-loud arm must not also emit the set summary —
    /// that is a second log line for one event — and a discarded `#[must_use]`
    /// reads as an oversight at the call site. Naming the intent keeps the
    /// default (`note`) the one that cannot silently produce an unreadable
    /// counter.
    pub fn bump(&self) {
        self.count.fetch_add(1, Ordering::Relaxed);
    }
}

/// Render a whole set as one bounded line: `<prefix> name=value name=value …`.
///
/// ⚠ The output is byte-compatible with the hand-written format string this
/// replaced, given the same prefix and the same counter order — which is what
/// makes the move checkable rather than merely plausible.
pub fn summary(prefix: &str, counters: &[&RefusalCounter]) -> String {
    let mut out = String::with_capacity(prefix.len() + counters.len() * 32);
    out.push_str(prefix);
    for c in counters {
        out.push(' ');
        out.push_str(c.name());
        out.push('=');
        out.push_str(&c.get().to_string());
    }
    out
}
