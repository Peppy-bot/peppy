//! Test-only scaffolding shared between this crate's unit tests and
//! downstream crates' tests (via the `test-support` feature).

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use crate::services::repo::cache::{
    self as repo_cache, ContractCacheEntry, EntryOrigin, McpExposureCacheEntry,
};
use daemon_config::consts::PeppyDirs;
use daemon_config::repository::{ItemName, ItemTag, ManifestFingerprint};
use tracing_subscriber::fmt::MakeWriter;

/// Cloneable in-memory sink for `tracing_subscriber`: pass a clone to
/// `with_writer` and read back everything logged via [`LogCapture::logs`].
#[derive(Clone, Default)]
pub struct LogCapture {
    buffer: Arc<parking_lot::Mutex<Vec<u8>>>,
}

impl LogCapture {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of everything captured so far.
    pub fn logs(&self) -> String {
        String::from_utf8(self.buffer.lock().clone()).expect("captured logs are valid UTF-8")
    }
}

// The capture is its own writer: clones share one buffer, so the subscriber
// can take a writer per event while readers snapshot the same log.
impl Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buffer.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogCapture {
    type Writer = LogCapture;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Scoped default subscriber for tests that *emit* tracing events (possibly
/// from spawned tasks on other threads) without asserting on them.
///
/// Registering a live `Dispatch` for the test's duration keeps
/// `tracing-core`'s callsite-interest cache computed over ALL registered
/// subscribers. With exactly one registered dispatcher, its `has_just_one`
/// fast path resolves a newly-hit callsite's interest against the *hitting
/// thread's* default instead — so a subscriber-less worker thread that fires
/// a shared callsite first would cache it never-enabled and silence a
/// concurrently-running test's `LogCapture` of that same callsite.
pub fn quiet_subscriber_guard() -> tracing::subscriber::DefaultGuard {
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(std::io::sink)
        .finish();
    tracing::subscriber::set_default(subscriber)
}

/// One document to seed a cache with: its identity and the file that
/// declares it, whose bytes are fingerprinted the way a refresh records
/// them.
pub type SeededDocument<'a> = (&'a str, &'a str, &'a Path);

/// Writes the contract cache of `dirs` as a refresh of one fs repository
/// publishing `documents` would leave it.
pub fn seed_contract_cache(dirs: &PeppyDirs, documents: &[SeededDocument<'_>]) {
    let entries: Vec<ContractCacheEntry> = documents
        .iter()
        .map(|(name, tag, path)| ContractCacheEntry {
            contract_name: ItemName::parse(name).expect("a valid contract name"),
            tag: ItemTag::parse(tag).expect("a valid contract tag"),
            sha256: fingerprint_of(path),
            origin: EntryOrigin::Fs {
                path: path.to_path_buf(),
            },
            repo_id: 0,
        })
        .collect();
    repo_cache::write_repo_cache(dirs, &entries).expect("the contract cache is written");
}

/// Writes the exposure cache of `dirs` as a refresh of one fs repository
/// publishing `documents` would leave it.
pub fn seed_exposure_cache(dirs: &PeppyDirs, documents: &[SeededDocument<'_>]) {
    let entries: Vec<McpExposureCacheEntry> = documents
        .iter()
        .map(|(name, tag, path)| McpExposureCacheEntry {
            exposure_name: ItemName::parse(name).expect("a valid exposure name"),
            tag: ItemTag::parse(tag).expect("a valid exposure tag"),
            sha256: fingerprint_of(path),
            origin: EntryOrigin::Fs {
                path: path.to_path_buf(),
            },
            repo_id: 0,
        })
        .collect();
    repo_cache::write_repo_cache(dirs, &entries).expect("the exposure cache is written");
}

fn fingerprint_of(path: &Path) -> ManifestFingerprint {
    let bytes = std::fs::read(path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    ManifestFingerprint::of_bytes(&bytes)
}
