//! Live VidMm tracker identities used to validate adopted-allocation charges.
//!
//! This table is an integrity check, not a KMT reference owner. The paired UMD
//! keeps the creator memory live through CreateAllocation, and an importer
//! opens the shared tracker (or a full-size fallback) before exposing the
//! imported resource.

#[derive(Clone, Copy)]
struct TrackerIdentity {
    cookie: u32,
    size: u64,
}

pub(crate) struct VidMmTrackerTable {
    entries: crate::sync::SpinLock<crate::sync::FixedVec<TrackerIdentity>>,
}

impl VidMmTrackerTable {
    const MAX_ENTRIES: usize = 8192;

    pub fn new() -> Self {
        Self {
            entries: crate::sync::SpinLock::new(crate::sync::FixedVec::with_max(Self::MAX_ENTRIES)),
        }
    }

    /// Register one live tracker. Cookies are unique so a payload cannot
    /// ambiguously attest against a different same-sized allocation.
    pub fn register(&self, cookie: u32, size: u64) -> bool {
        if cookie == 0 || size == 0 {
            return false;
        }
        let mut entries = self.entries.lock();
        if entries
            .as_slice()
            .iter()
            .any(|entry| entry.cookie == cookie)
        {
            return false;
        }
        entries.push(TrackerIdentity { cookie, size })
    }

    pub fn matches(&self, cookie: u32, size: u64) -> bool {
        cookie != 0
            && self
                .entries
                .lock()
                .as_slice()
                .iter()
                .any(|entry| entry.cookie == cookie && entry.size == size)
    }

    pub fn remove(&self, cookie: u32) {
        if cookie == 0 {
            return;
        }
        let mut entries = self.entries.lock();
        if let Some(index) = entries
            .as_slice()
            .iter()
            .position(|entry| entry.cookie == cookie)
        {
            entries.swap_remove(index);
        }
    }
}
