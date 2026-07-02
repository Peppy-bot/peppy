//! Per-key in-process locks used to serialize concurrent work on the same
//! cache directory.
//!
//! Each cache module declares its own `static LOCKS: KeyedLocks` so the git
//! and bundle caches keep separate lock namespaces (their keys are directory
//! paths under different roots, but sharing a map would still couple their
//! GC and contention behavior for no benefit).

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// A lazily-initialized map from string keys to per-key mutexes. Designed to
/// live in a `static`, so construction is `const`.
pub(super) struct KeyedLocks(OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>>);

impl KeyedLocks {
    pub(super) const fn new() -> Self {
        Self(OnceLock::new())
    }

    /// Returns the mutex for `key`, creating it on first use.
    pub(super) fn lock_for(&self, key: &str) -> Arc<Mutex<()>> {
        let mut map = self.0.get_or_init(|| Mutex::new(HashMap::new())).lock();
        // GC entries not currently held by any caller. `strong_count == 1`
        // means only the map still references the Arc, so no one can race on
        // rebuilding the slot (the map lock serializes all access).
        map.retain(|_, v| Arc::strong_count(v) > 1);
        map.entry(key.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}
