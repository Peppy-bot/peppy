//! Test-only scaffolding shared between this crate's unit tests and
//! downstream crates' tests (via the `test-support` feature).

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::services::repo::cache::{
    self as repo_cache, ContractCacheEntry, EntryOrigin, McpExposureCacheEntry,
};
use daemon_config::consts::PeppyDirs;
use daemon_config::repository::{ItemName, ItemTag, ManifestFingerprint};
use tracing_subscriber::fmt::MakeWriter;

use crate::RestartBackoff;

/// A backoff with no wait and no reset, for a test that needs the respawn to
/// happen.
pub fn instant_restart_backoff() -> RestartBackoff {
    RestartBackoff {
        first_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
        reset_after: Duration::MAX,
    }
}

/// A backoff whose wait outlasts any test, for a test that acts during the
/// wait.
pub fn parked_restart_backoff() -> RestartBackoff {
    RestartBackoff {
        first_delay: Duration::from_secs(3600),
        max_delay: Duration::from_secs(3600),
        reset_after: Duration::MAX,
    }
}

/// A backoff with no wait whose reset window is [`RESET_WINDOW`], for a test
/// of the reset: a run shorter than the window backs off further, a longer
/// one starts it over.
pub fn resetting_restart_backoff() -> RestartBackoff {
    RestartBackoff {
        first_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
        reset_after: RESET_WINDOW,
    }
}

/// The reset window of [`resetting_restart_backoff`]: long enough that a
/// kill right after a start lands inside it on a loaded machine, and shorter
/// than the time a test monitor takes to kill a process that never answers,
/// so a hang loop's runs pass it by the clock while their answers, which
/// count, stay inside it.
pub const RESET_WINDOW: Duration = Duration::from_secs(1);

/// The shell body of a test process that runs until SIGTERM and then exits
/// 0.
pub const EXIT_CLEANLY_ON_TERM: &str = "trap 'exit 0' TERM; while true; do sleep 0.2; done";

/// The `framework` entry of a test manifest, `framework: <block>,`, or
/// nothing.
pub fn framework_entry(framework: Option<&str>) -> String {
    framework.map_or(String::new(), |block| format!("framework: {block},"))
}

/// The shell command that copies a process's runtime config to
/// `<dumps>/<pid>.json5`.
pub fn dump_runtime_config(dumps: &Path) -> String {
    format!(
        r#"cp "$PEPPY_RUNTIME_CONFIG" '{}/'$$.json5"#,
        dumps.display()
    )
}

/// How long [`poll_file`] waits for a test process to write its file.
const FILE_WRITTEN_WITHIN: Duration = Duration::from_secs(15);

/// What a test process writes to `path`, once `parse` reads it whole: a
/// read that races the write returns part of the file.
pub fn poll_file<T>(path: &Path, parse: impl Fn(&str) -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + FILE_WRITTEN_WITHIN;
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Some(parsed) = parse(&text)
        {
            return parsed;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{} is written whole",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Sends `signal` (`-9`, `-TERM`) to the test process `pid`.
pub fn signal_process(pid: u32, signal: &str) {
    let status = std::process::Command::new("kill")
        .args([signal, &pid.to_string()])
        .status()
        .expect("kill runs");
    assert!(status.success(), "kill {signal} {pid}");
}

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

    /// Captures this thread's events, uncolored and untimed, until the
    /// returned guard drops.
    pub fn install(&self) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_writer(self.clone())
                .finish(),
        )
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
/// thread's* default instead, so a subscriber-less worker thread that fires
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
