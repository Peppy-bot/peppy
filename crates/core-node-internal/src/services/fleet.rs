//! The fleet of the stack that this daemon coordinates: the leases of its
//! robots, the halt, and the clients that follow its changes. The book
//! ([`FleetBook`]) holds every rule; [`Fleet`] holds the book for the
//! daemon, and keeps the halt on disk so it stays after a restart.

mod book;

pub(crate) use book::{ClientRef, FleetBook, Refusal, RefusalKind};

use config::failure_kind::PeppyFailureKind;
use daemon_config::daemon_interface::fleet::FleetFailureKind;
use peppylib::messaging::Failure;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The book of the fleet, and the file that keeps its halt.
#[derive(Default)]
pub(crate) struct Fleet {
    book: parking_lot::Mutex<FleetBook>,
    halt_file: OnceLock<PathBuf>,
}

/// The halt as the daemon writes it to disk.
#[derive(Debug, Serialize, Deserialize)]
struct WrittenHalt {
    halted_by: String,
}

impl std::fmt::Debug for Fleet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fleet")
            .field("halt_file", &self.halt_file.get())
            .finish_non_exhaustive()
    }
}

impl Fleet {
    /// Keeps the halt in `path` from now on, and reads the halt that a
    /// daemon before this one wrote there. A file that does not read is
    /// read as a halt: a halt that the daemon cannot read stays a halt.
    pub(crate) fn keep_halt_in(&self, path: PathBuf) {
        let halted_by = read_halt(&path);
        if let Some(by) = &halted_by {
            tracing::warn!(%by, "the stack is halted: a client halted it before this daemon started");
        }
        *self.book.lock() = FleetBook::with_halt(halted_by);
        if self.halt_file.set(path).is_err() {
            tracing::warn!("the fleet keeps its halt in one file, set once");
        }
    }

    /// Runs `work` on the book.
    pub(crate) fn with<R>(&self, work: impl FnOnce(&mut FleetBook) -> R) -> R {
        work(&mut self.book.lock())
    }

    /// Halts the stack for `by` and writes the halt to disk. Answers whether
    /// the halt started now.
    pub(crate) fn halt(&self, by: &str) -> std::io::Result<bool> {
        let mut book = self.book.lock();
        if book.halted_by().is_some() {
            return Ok(false);
        }
        if let Some(path) = self.halt_file.get() {
            write_halt(path, by)?;
        }
        Ok(book.halt(by))
    }

    /// Ends the halt and removes it from disk. Answers whether a halt ended.
    pub(crate) fn clear_halt(&self) -> std::io::Result<bool> {
        let mut book = self.book.lock();
        if book.halted_by().is_none() {
            return Ok(false);
        }
        if let Some(path) = self.halt_file.get() {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(book.clear_halt())
    }
}

/// The client that halted the stack, as `path` keeps it; `None` when no
/// halt is written there.
fn read_halt(path: &Path) -> Option<String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "the halt file does not read; the stack stays halted");
            return Some("a halt that this daemon could not read".to_owned());
        }
    };
    match serde_json5::from_str::<WrittenHalt>(&text) {
        Ok(halt) => Some(halt.halted_by),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "the halt file does not parse; the stack stays halted");
            Some("a halt that this daemon could not read".to_owned())
        }
    }
}

fn write_halt(path: &Path, by: &str) -> std::io::Result<()> {
    let text = json5_pretty::to_string_pretty(&WrittenHalt {
        halted_by: by.to_owned(),
    })
    .map_err(std::io::Error::other)?;
    daemon_config::atomic_write::publish_atomic(path, |tmp| std::fs::write(tmp, text.as_bytes()))
        .map(|_| ())
}

impl Refusal {
    /// The refusal as the error reply of a service or the refusal of a
    /// goal.
    pub(crate) fn into_failure(self) -> Failure {
        match self.kind {
            RefusalKind::Fleet(kind) => Failure::new(kind, self.message),
            RefusalKind::PreconditionFailed => {
                Failure::new(PeppyFailureKind::PreconditionFailed, self.message)
            }
            RefusalKind::Rejected => Failure::new(PeppyFailureKind::Rejected, self.message),
        }
    }

    /// The kind of the daemon that a stack change gives for this refusal:
    /// a join or a removal that a lease or the halt refuses.
    pub(crate) fn into_daemon_failure(self) -> Failure {
        let kind = match self.kind {
            RefusalKind::Fleet(FleetFailureKind::Halted) => {
                core_node_api::DaemonFailureKind::Halted
            }
            RefusalKind::Fleet(FleetFailureKind::Leased) => {
                core_node_api::DaemonFailureKind::Leased
            }
            RefusalKind::Fleet(FleetFailureKind::NotLeased) | RefusalKind::PreconditionFailed => {
                PeppyFailureKind::PreconditionFailed.into()
            }
            RefusalKind::Rejected => PeppyFailureKind::Rejected.into(),
        };
        Failure::new(kind, self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_halt_stays_on_disk_until_it_is_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet_halt.json5");
        let fleet = Fleet::default();
        fleet.keep_halt_in(path.clone());
        assert_eq!(fleet.with(|book| book.halted_by().map(str::to_owned)), None);
        assert!(fleet.halt("peppy-cli on cn-one").unwrap());
        assert!(
            !fleet.halt("another").unwrap(),
            "a second halt changes nothing"
        );

        let restarted = Fleet::default();
        restarted.keep_halt_in(path.clone());
        assert_eq!(
            restarted.with(|book| book.halted_by().map(str::to_owned)),
            Some("peppy-cli on cn-one".to_owned()),
            "the halt stays after a restart of the daemon"
        );
        assert!(restarted.clear_halt().unwrap());
        assert!(!path.exists());
        assert!(
            !restarted.clear_halt().unwrap(),
            "a clear without a halt changes nothing"
        );

        let cleared = Fleet::default();
        cleared.keep_halt_in(path.clone());
        assert_eq!(
            cleared.with(|book| book.halted_by().map(str::to_owned)),
            None
        );
    }

    #[test]
    fn a_halt_file_that_does_not_parse_keeps_the_stack_halted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet_halt.json5");
        std::fs::write(&path, "{ not json5").unwrap();
        let fleet = Fleet::default();
        fleet.keep_halt_in(path);
        assert!(fleet.with(|book| book.halted_by().is_some()));
    }
}
