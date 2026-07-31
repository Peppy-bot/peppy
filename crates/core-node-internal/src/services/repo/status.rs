//! Per-repository read status, written to `~/.peppy/cache/repo_status.json5`
//! by every refresh.
//!
//! Containment lets a repository keep serving the entries it last
//! published while later refreshes fail. That is the right default, since
//! the alternative is losing identities that launchers reference, but it
//! means a repository failing for a month is still serving month-old
//! entries. Recording when each repository was last read successfully is
//! what keeps that visible instead of a machine sitting on stale bytes
//! believing it is current.
//!
//! This lives beside the four entry caches rather than as fields on the
//! entries themselves: it is per repository, not per entry, and keeping
//! it separate leaves the entry caches' on-disk shape untouched.
//!
//! Deliberately records a timestamp and not a revision. Knowing *when*
//! entries were last read is enough for everything here; recording which
//! commit they came from is a larger change to what a cache holds.

use crate::Result;
use core_node_api::encoding::RepoSourceKind;
use daemon_config::consts::PeppyDirs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

pub(crate) const FILE_NAME: &str = "repo_status.json5";

/// How the most recent read of a repository went wrong.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RepoStatusFailure {
    /// `"unreachable"` or `"conflict"`. Text on disk so the file stays
    /// readable and an unknown value from a future peppy degrades to a
    /// label rather than a parse error.
    pub kind: String,
    pub message: String,
    pub unix_secs: u64,
}

/// One repository's read history, as of the last refresh.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RepoStatus {
    pub id: u64,
    pub identity: String,
    pub source_type: RepoSourceKind,
    /// Unix seconds of the last read that produced entries. `None` on a
    /// machine that has never read this repository successfully, which is
    /// exactly the fresh-machine case where there is nothing to fall back
    /// to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_read_unix_secs: Option<u64>,
    /// Absent once a read succeeds, so a repository that recovered does
    /// not keep reporting an old failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<RepoStatusFailure>,
}

impl RepoStatus {
    /// Whether the entries currently in the caches for this repository
    /// came from an earlier run than this one.
    pub(crate) fn is_retained(&self) -> bool {
        self.last_failure.is_some()
    }
}

pub(crate) fn repo_status_path(peppy_dirs: &PeppyDirs) -> PathBuf {
    peppy_dirs.cache_dir().join(FILE_NAME)
}

/// Unix seconds for `now`, saturating at the epoch for clocks set before
/// it. A status file is a diagnostic, so a nonsense clock degrades the
/// reporting rather than failing the refresh that produced it.
pub(crate) fn unix_secs(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Reads the status file, or an empty list when it is missing or
/// unreadable. Never an error: this is diagnostic data, and refusing to
/// refresh because the diagnostics are corrupt helps nobody.
pub(crate) fn read(peppy_dirs: &PeppyDirs) -> Vec<RepoStatus> {
    let path = repo_status_path(peppy_dirs);
    if !path.exists() {
        return Vec::new();
    }
    let parsed = std::fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|content| serde_json5::from_str::<Vec<RepoStatus>>(&content).map_err(|e| e.to_string()));
    match parsed {
        Ok(statuses) => statuses,
        Err(e) => {
            warn!("Could not read {} at {}: {e}", FILE_NAME, path.display());
            Vec::new()
        }
    }
}

/// Publishes the status file atomically, like the entry caches, so a
/// concurrent `repo list` never observes a partial file.
pub(crate) fn write(peppy_dirs: &PeppyDirs, statuses: &[RepoStatus]) -> Result<()> {
    let content = json5_pretty::to_string_pretty(statuses).map_err(|e| {
        core_node_api::Error::Encoding(format!("failed to serialize {FILE_NAME}: {e}"))
    })?;
    daemon_config::atomic_write::publish_atomic(&repo_status_path(peppy_dirs), |tmp| {
        std::fs::write(tmp, &content)
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(id: u64) -> RepoStatus {
        RepoStatus {
            id,
            identity: format!("https://example.com/{id}.git"),
            source_type: RepoSourceKind::Git,
            last_read_unix_secs: Some(1_753_900_000),
            last_failure: None,
        }
    }

    #[test]
    fn write_then_read_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        std::fs::create_dir_all(peppy_dirs.cache_dir()).unwrap();

        let mut failed = status(1001);
        failed.last_failure = Some(RepoStatusFailure {
            kind: "conflict".to_owned(),
            message: "two node manifests claim `a:v1`".to_owned(),
            unix_secs: 1_753_986_400,
        });
        let written = vec![status(1000), failed];

        write(&peppy_dirs, &written).unwrap();

        assert_eq!(read(&peppy_dirs), written);
    }

    #[test]
    fn read_returns_empty_when_file_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read(&PeppyDirs::new(tmp.path())).is_empty());
    }

    /// A corrupt status file must not stop a refresh: it holds
    /// diagnostics, not the entries themselves.
    #[test]
    fn read_returns_empty_when_file_is_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        std::fs::create_dir_all(peppy_dirs.cache_dir()).unwrap();
        std::fs::write(repo_status_path(&peppy_dirs), "not json5 at all {{{").unwrap();

        assert!(read(&peppy_dirs).is_empty());
    }

    /// A repository is "retained" exactly while its last read failed;
    /// once it reads cleanly the failure is cleared and it is current.
    #[test]
    fn is_retained_tracks_the_last_failure() {
        let mut s = status(1000);
        assert!(!s.is_retained());
        s.last_failure = Some(RepoStatusFailure {
            kind: "unreachable".to_owned(),
            message: "network is unreachable".to_owned(),
            unix_secs: 1_753_986_400,
        });
        assert!(s.is_retained());
    }

    /// Timestamps come from a caller-supplied `SystemTime` so tests stay
    /// deterministic, and a clock set before the epoch degrades to 0
    /// rather than failing.
    #[test]
    fn unix_secs_converts_and_saturates() {
        assert_eq!(
            unix_secs(UNIX_EPOCH + std::time::Duration::from_secs(1_753_900_000)),
            1_753_900_000
        );
        assert_eq!(
            unix_secs(UNIX_EPOCH - std::time::Duration::from_secs(60)),
            0,
            "a clock set before the epoch degrades rather than panicking"
        );
    }
}
