//! The json5 documents this crate keeps on disk: the credentials
//! ([`crate::storage`]), the enrollment record ([`crate::enrollment`]) and the
//! context ([`crate::context`]). Each one carries a schema version, and there
//! is one reader per document, for one version: a file of another version is
//! rejected, and the message names the command that writes the document anew.

use std::path::Path;

use serde::{Serialize, de::DeserializeOwned};

use crate::error::{Error, Result};
use crate::fs_perms::restrict_file;

/// A document with a schema version.
pub(crate) trait Versioned {
    /// The one version this crate reads and writes.
    const VERSION: u32;
    /// The kind of document, as a message names it: `credentials`,
    /// `enrollment`, `context`.
    const WHAT: &'static str;
    /// The command that writes the document anew.
    const REMEDY: &'static str;

    /// The version the file carries; `0` when it carries none.
    fn version(&self) -> u32;
}

/// Reads the document at `path`. `Ok(None)` when the file is absent. An error
/// when it is present but does not parse or is of another version; the
/// message names the remedy.
pub(crate) fn load<T: DeserializeOwned + Versioned>(path: &Path) -> Result<Option<T>> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io(e)),
    };
    let document: T = serde_json5::from_str(&content).map_err(|e| {
        Error::Auth(format!(
            "failed to parse {}: {e}; run `{}` again",
            path.display(),
            T::REMEDY
        ))
    })?;
    if document.version() != T::VERSION {
        return Err(Error::Auth(format!(
            "{} file {} is an unsupported format (v{}, expected v{}); run `{}` again",
            T::WHAT,
            path.display(),
            document.version(),
            T::VERSION,
            T::REMEDY
        )));
    }
    Ok(Some(document))
}

/// Writes `document` to `path` atomically, and owner-only when `owner_only`.
pub(crate) fn save<T: Serialize + Versioned>(
    path: &Path,
    document: &T,
    owner_only: bool,
) -> Result<()> {
    let content = json5_pretty::to_string_pretty(document)
        .map_err(|e| Error::Auth(format!("failed to serialize the {}: {e}", T::WHAT)))?;
    publish(path, &content, owner_only)
}

/// Writes `content` to `path` atomically, and owner-only when `owner_only`.
pub(crate) fn publish(path: &Path, content: &str, owner_only: bool) -> Result<()> {
    daemon_config::atomic_write::publish_atomic(path, |tmp| {
        std::fs::write(tmp, content)?;
        if owner_only {
            restrict_file(tmp)?;
        }
        Ok(())
    })?;
    Ok(())
}

/// Removes the file at `path`. Absent is not an error: there is then no
/// document.
pub(crate) fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}
