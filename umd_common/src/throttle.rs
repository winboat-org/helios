//! Occurrence counting for log sites whose budget is a call argument rather
//! than a constant baked into the static.
//!
//! Moved verbatim out of `umd/src/forward.rs` by the `umd_common` extraction
//! (`DECISIONS.md` D3b). The only change is visibility: the type and its
//! methods were `pub(super)` inside one crate and are `pub` here, because two
//! cdylibs now consume them. **No behaviour changed** — the ordinal arithmetic
//! and the `Ordering::Relaxed` accesses are byte-for-byte what they were.
//!
//! The budget stays a call argument deliberately: baking it into the static
//! would make every site's rate a property of this file instead of a property
//! of the site, which is what made the pre-R8xx logging impossible to reason
//! about.

use core::sync::atomic::{AtomicUsize, Ordering};

pub struct LogThrottle {
    count: AtomicUsize,
}

impl LogThrottle {
    pub const fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
        }
    }

    /// Bump and return the occurrence ordinal with no rate decision, for sites
    /// whose gate carries an extra escape clause (`|| alloc != 0`) or a shape
    /// of its own.
    pub fn next(&self) -> usize {
        self.count.fetch_add(1, Ordering::Relaxed)
    }

    /// Read the ordinal WITHOUT bumping it — one site logs a "pre" line under
    /// the same budget as the "post" line that follows it.
    pub fn peek(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    /// The first `first` occurrences.
    pub fn first_n(&self, first: usize) -> Option<usize> {
        let n = self.next();
        (n < first).then_some(n)
    }

    /// The first `first`, then every `every`-th counting from zero.
    pub fn first_n_then_every(&self, first: usize, every: usize) -> Option<usize> {
        let n = self.next();
        (n < first || n % every == 0).then_some(n)
    }

    /// The first `first`, then every `every`-th counting from one. Distinct
    /// from [`Self::first_n_then_every`]: it fires at n = every-1, 2*every-1,
    /// not at n = 0, every, 2*every.
    pub fn first_n_then_every_from_one(&self, first: usize, every: usize) -> Option<usize> {
        let n = self.next();
        (n < first || (n + 1) % every == 0).then_some(n)
    }
}

impl Default for LogThrottle {
    fn default() -> Self {
        Self::new()
    }
}
