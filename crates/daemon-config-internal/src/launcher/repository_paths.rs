//! The paths a launcher document names beside itself: the fragment files of
//! its options and the files of its includes. Both follow one rule: a path is
//! relative to the directory of the document that writes it, and the written
//! path and its resolved symlink target stay inside the launcher's
//! repository.

use std::path::{Component as PathComponent, Path, PathBuf};

/// The canonical directory of a launcher and the boundary of its
/// repository: the nearest ancestor that holds a valid
/// `peppy_repository.json5`, or the launcher's own directory for a
/// standalone launcher.
#[derive(Debug, Clone)]
pub(super) struct LauncherPaths {
    pub(super) directory: PathBuf,
    pub(super) boundary: PathBuf,
}

/// Why a written path is not usable.
#[derive(Debug)]
pub(super) enum PathRefusal {
    /// The written path breaks the rule: the reason, as "it ...".
    Rule(&'static str),
    /// The path names no file that can be resolved: the error of the
    /// filesystem.
    Unresolved(std::io::Error),
}

impl LauncherPaths {
    /// The paths of the launcher at `file`, inside the repository of the
    /// nearest `peppy_repository.json5` above it. The error says why the
    /// launcher's directory cannot be used.
    pub(super) fn of_launcher(file: &Path) -> Result<Self, String> {
        let directory = canonical_directory_of(file)?;
        for ancestor in directory.ancestors() {
            let index = ancestor.join(crate::consts::REPOSITORY_INDEX_FILE);
            match index.try_exists() {
                Ok(false) => continue,
                Err(error) => {
                    return Err(format!("{} cannot be read: {error}", index.display()));
                }
                Ok(true) => {}
            }
            crate::repository::PeppyRepositoryIndexParser::from_path(&index)
                .map_err(|error| format!("{} does not parse: {error}", index.display()))?;
            return Ok(Self {
                boundary: ancestor.to_path_buf(),
                directory,
            });
        }
        Ok(Self {
            boundary: directory.clone(),
            directory,
        })
    }

    /// The paths of the launcher at `file` of the repository whose root is
    /// `root`, as the walk of that repository reads it: the root is the
    /// boundary, whether or not it holds its `peppy_repository.json5` yet.
    pub(super) fn in_repository(file: &Path, root: &Path) -> Result<Self, String> {
        let directory = canonical_directory_of(file)?;
        let boundary = root
            .canonicalize()
            .map_err(|error| format!("{} cannot be resolved: {error}", root.display()))?;
        if !directory.starts_with(&boundary) {
            return Err(format!(
                "{} is outside the repository {}",
                directory.display(),
                boundary.display()
            ));
        }
        Ok(Self {
            directory,
            boundary,
        })
    }
}

/// The canonical directory of the document at `file`.
pub(super) fn canonical_directory_of(file: &Path) -> Result<PathBuf, String> {
    let directory = directory_of(file);
    directory
        .canonicalize()
        .map_err(|error| format!("{} cannot be resolved: {error}", directory.display()))
}

/// The directory of the document at `file`: its parent, or the working
/// directory when the path has none.
fn directory_of(file: &Path) -> PathBuf {
    match file.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// The canonical file that `raw`, written in a document of `directory`,
/// names. `directory` is canonical and inside the boundary of `paths`. Both
/// the written path and the resolved symlink target must stay inside the
/// launcher's repository.
pub(super) fn resolve_repository_path(
    paths: &LauncherPaths,
    directory: &Path,
    raw: &str,
) -> Result<PathBuf, PathRefusal> {
    let relative = Path::new(raw);
    if relative.is_absolute() {
        return Err(PathRefusal::Rule("it is absolute"));
    }
    // `Path::components` drops a `.` segment that is not the first one, so
    // the written segments are read here.
    if raw.split('/').any(|segment| segment == ".") {
        return Err(PathRefusal::Rule("it contains a `.` segment"));
    }
    let mut cleaned = directory.to_path_buf();
    for component in relative.components() {
        match component {
            PathComponent::Normal(segment) => cleaned.push(segment),
            PathComponent::ParentDir => {
                if cleaned == paths.boundary {
                    return Err(PathRefusal::Rule("it leaves the launcher's repository"));
                }
                cleaned.pop();
            }
            _ => return Err(PathRefusal::Rule("it is not a plain relative path")),
        }
    }
    let canonical = directory
        .join(relative)
        .canonicalize()
        .map_err(PathRefusal::Unresolved)?;
    if !canonical.starts_with(&paths.boundary) {
        return Err(PathRefusal::Rule(
            "it leaves the launcher's repository through a symlink",
        ));
    }
    Ok(canonical)
}
