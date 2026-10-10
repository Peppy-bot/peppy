use crate::error::{Error, Result};
use askama::Template;
use rust_embed::Embed;
use std::{
    fs,
    io::{self, ErrorKind},
    path::Path,
};

#[derive(Embed)]
#[folder = "templates/"]
struct EmbeddedTemplates;

#[derive(Template)]
#[template(path = "peppygen/rust/Cargo.toml.j2", escape = "none")]
struct PeppyConfigTemplate<'a> {
    peppylib_version: &'a str,
    peppylib_path: &'a str,
}

impl<'a> PeppyConfigTemplate<'a> {
    pub const TEMPLATE_PATH: &'static str = "peppygen/rust/Cargo.toml.j2";
}

#[derive(Template)]
#[template(path = "peppygen/python/pyproject.toml.j2", escape = "none")]
struct PeppyPythonConfigTemplate<'a> {
    peppylib_version: &'a str,
    python_min_version: &'a str,
    python_max_version: &'a str,
}

impl PeppyPythonConfigTemplate<'_> {
    pub const TEMPLATE_PATH: &'static str = "peppygen/python/pyproject.toml.j2";
}

/// Version of the deployed peppylib Python distribution.
///
/// A PEP 440 local version: public indexes reject local versions, so
/// peppygen's exact `peppylib==<this>` requirement can never be satisfied
/// from PyPI; a node missing the `.peppy/libs/peppylib` path source fails
/// loudly instead of silently installing an outdated release. Intentionally
/// differs from `peppylib.__version__` (which is stamped from PEPPY_GIT_TAG).
pub(crate) const PYTHON_PEPPYLIB_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+peppy");

#[derive(Template)]
#[template(path = "peppylib/python/pyproject.toml.j2", escape = "none")]
struct PeppylibPythonProjectTemplate<'a> {
    peppylib_version: &'a str,
    python_min_version: &'a str,
    python_max_version: &'a str,
}

/// Renders the manifest of the standalone peppylib Python project deployed
/// at `.peppy/libs/peppylib/pyproject.toml`. Plain setuptools packaging of
/// the prebuilt wrappers + native extension, never maturin, since end-user
/// machines have no Rust toolchain.
pub(crate) fn render_peppylib_python_pyproject() -> Result<String> {
    let tpl = PeppylibPythonProjectTemplate {
        peppylib_version: PYTHON_PEPPYLIB_VERSION,
        python_min_version: config::consts::PYTHON_MIN_VERSION,
        python_max_version: config::consts::PYTHON_MAX_VERSION,
    };
    Ok(tpl.render()?)
}

pub(crate) fn copy_embedded_templates(prefix: &str, to: &Path, peppylib_path: &str) -> Result<()> {
    let prefix_with_slash = if prefix.ends_with('/') {
        prefix.to_string()
    } else {
        format!("{}/", prefix)
    };

    for file_path in EmbeddedTemplates::iter() {
        let file_path_str = file_path.as_ref();

        // Only process files under the specified prefix
        let Some(relative_path) = file_path_str.strip_prefix(&prefix_with_slash) else {
            continue;
        };

        // Skip .gitkeep files
        if relative_path.ends_with(".gitkeep") {
            continue;
        }

        let destination = if relative_path.ends_with(".j2") {
            // Remove the .j2 extension for template files
            let without_ext = relative_path.strip_suffix(".j2").unwrap_or(relative_path);
            to.join(without_ext)
        } else {
            to.join(relative_path)
        };

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }

        if relative_path.ends_with(".j2") {
            // Render template files
            let rendered = render_template(file_path_str, peppylib_path)?;
            fs::write(&destination, rendered)?;
        } else {
            // Copy regular files
            let content = EmbeddedTemplates::get(file_path_str).ok_or_else(|| {
                io::Error::new(
                    ErrorKind::NotFound,
                    format!("embedded template not found: {file_path_str}"),
                )
            })?;
            fs::write(&destination, content.data.as_ref())?;
        }
    }

    Ok(())
}

fn render_template(template_path: &str, peppylib_path: &str) -> Result<String> {
    match template_path {
        PeppyConfigTemplate::TEMPLATE_PATH => {
            let tpl = PeppyConfigTemplate {
                peppylib_version: env!("CARGO_PKG_VERSION"),
                peppylib_path,
            };
            Ok(tpl.render()?)
        }
        PeppyPythonConfigTemplate::TEMPLATE_PATH => {
            let tpl = PeppyPythonConfigTemplate {
                peppylib_version: PYTHON_PEPPYLIB_VERSION,
                python_min_version: config::consts::PYTHON_MIN_VERSION,
                python_max_version: config::consts::PYTHON_MAX_VERSION,
            };
            Ok(tpl.render()?)
        }
        _ => Err(Error::UnknownTemplate(template_path.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Embedded crate sources for Rust dependency vendoring
//
// The `$PEPPY_SHARED_DIR` in each `#[folder]` is expanded at compile time by
// rust-embed (the `interpolate-folder-path` feature). generator's build script
// sets it to the `peppy-shared` directory located via build-helpers, which
// knows where its own tree sits, so no `../../../` reach from here is
// required.
// ---------------------------------------------------------------------------

#[derive(Embed)]
#[folder = "$PEPPY_SHARED_DIR/peppylib-rs/"]
#[include = "*.rs"]
#[include = "*.toml"]
#[include = "*.capnp"]
#[include = "*.j2"]
#[exclude = "target/*"]
#[exclude = "tests/*"]
#[exclude = "examples/*"]
pub(crate) struct EmbeddedPeppylib;

#[derive(Embed)]
#[folder = "$PEPPY_SHARED_DIR/peppy-messaging-interface/"]
#[include = "*.rs"]
#[include = "*.toml"]
#[include = "*.capnp"]
#[include = "*.j2"]
#[exclude = "target/*"]
#[exclude = "tests/*"]
#[exclude = "examples/*"]
pub(crate) struct EmbeddedPeppyMessagingInterface;

#[derive(Embed)]
#[folder = "$PEPPY_SHARED_DIR/peppy-config-model/"]
#[include = "*.rs"]
#[include = "*.toml"]
#[include = "*.capnp"]
#[include = "*.j2"]
#[include = "tools/capnp_*"]
#[exclude = "target/*"]
#[exclude = "tests/*"]
#[exclude = "examples/*"]
pub(crate) struct EmbeddedConfig;

#[derive(Embed)]
#[folder = "$PEPPY_SHARED_DIR/build-helpers/"]
#[include = "*.rs"]
#[include = "*.toml"]
#[exclude = "target/*"]
#[exclude = "tests/*"]
pub(crate) struct EmbeddedBuildHelpers;

#[derive(Embed)]
#[folder = "$PEPPY_SHARED_DIR/core-node-api/"]
#[include = "*.rs"]
#[include = "*.toml"]
#[include = "*.capnp"]
#[exclude = "target/*"]
#[exclude = "tests/*"]
#[exclude = "examples/*"]
pub(crate) struct EmbeddedCoreNodeApi;

/// Recursively copies a directory and all of its contents.
pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let dest_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &dest_path)?;
        } else {
            fs::copy(entry.path(), &dest_path)?;
        }
    }
    Ok(())
}

/// Returns a sibling path of `cache_dir` by appending `suffix` to its full name.
///
/// Unlike `Path::with_extension`, this preserves dots in the original name.
/// For example, given `some/path/abc123-1.0.0` and suffix `.lock`, this returns
/// `some/path/abc123-1.0.0.lock` (not `some/path/abc123-1.0.lock`).
fn cache_sibling_path(cache_dir: &Path, suffix: &str) -> std::path::PathBuf {
    let name = cache_dir
        .file_name()
        .expect("cache_dir must have a file name component");
    let mut new_name = name.to_os_string();
    new_name.push(suffix);
    cache_dir.with_file_name(new_name)
}

/// Fills the shared cache directory `cache_dir` once, safe against the
/// generations of other processes that fill the same cache at the same time.
///
/// A `.complete` marker inside `cache_dir` states that the cache is whole. It
/// is written last and never removed, so a caller that sees it reads a whole
/// cache without taking the lock. Otherwise the `.lock` sibling is locked and
/// the marker checked again, because another process may have finished the
/// cache while this one waited. `fill` then writes the cache into a
/// `.staging-<pid>` sibling, which replaces whatever partial `cache_dir` an
/// interrupted run left before the marker is written.
pub(crate) fn populate_cache_dir_once(
    cache_dir: &Path,
    fill: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let complete_marker = cache_dir.join(".complete");
    if complete_marker.exists() {
        return Ok(());
    }

    let parent = cache_dir
        .parent()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "cache dir has no parent"))?;
    fs::create_dir_all(parent)?;
    let lock_file = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(cache_sibling_path(cache_dir, ".lock"))?;
    lock_file.lock()?;
    if complete_marker.exists() {
        return Ok(());
    }

    let staging_dir = cache_sibling_path(cache_dir, &format!(".staging-{}", std::process::id()));
    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir)?;
    }
    fill(&staging_dir)?;
    if cache_dir.exists() {
        fs::remove_dir_all(cache_dir)?;
    }
    fs::rename(&staging_dir, cache_dir)?;
    fs::write(&complete_marker, "")?;
    // The lock is released only once the marker is written.
    drop(lock_file);
    Ok(())
}

// ---------------------------------------------------------------------------
// Crate deployment mode
// ---------------------------------------------------------------------------

/// Controls how vendored crates are linked into a node's `.peppy/libs/` directory.
///
/// `Symlink` (the default) creates symlinks to a shared cache: fast and avoids
/// duplicating files across nodes on the host filesystem.
///
/// `Copy` physically copies the crate sources into the node directory.
/// This is required for container builds where Apptainer's `%files` section
/// copies symlinks as-is, breaking absolute symlinks that point to host paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CrateDeployMode {
    #[default]
    Symlink,
    Copy,
}

/// What the tree a generation writes into is, which decides whether the
/// generation records the peppy build hash in `.peppy/git.hash` and whether
/// the generated Python test harness bakes the tree's absolute path as its
/// last-resort node directory (see `peppylib.testing.resolve_node_dir`).
///
/// Generated code has to be a pure function of the node's sources: the
/// daemon fingerprints a staged tree to reuse build artifacts, so a path
/// into a per-add staging directory, or the hash of the daemon that staged
/// it, would make byte-identical sources build again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NodeTree {
    /// The node's own source tree, which stays where it is: `git.hash`
    /// records which peppy build generated it, so a later `node add` can
    /// tell a stale generation, and the harness bakes the tree's path so a
    /// test run from any working directory finds `peppy.json5`.
    #[default]
    Source,
    /// A copy staged for one build and deleted afterwards: no `git.hash`,
    /// since nothing reads it there and it would key the build artifact on
    /// the daemon's commit; and no baked path, since one into the staging
    /// directory would dangle.
    Staged,
}

// ---------------------------------------------------------------------------
// Shared crate-vendoring utilities
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct WorkspacePackageMetadata {
    pub version: &'static str,
    pub edition: &'static str,
}

impl WorkspacePackageMetadata {
    pub const fn embedded() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
            edition: "2024",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn write_file(dir: &Path, name: &str, contents: &str) -> Result<()> {
        fs::create_dir_all(dir)?;
        fs::write(dir.join(name), contents)?;
        Ok(())
    }

    #[test]
    fn populate_cache_dir_once_fills_the_cache_and_marks_it_complete() {
        let root = TempDir::new().unwrap();
        let cache_dir = root.path().join("caches").join("abc123-1.0.0");

        populate_cache_dir_once(&cache_dir, |staging| write_file(staging, "lib.txt", "v1"))
            .unwrap();

        assert_eq!(fs::read_to_string(cache_dir.join("lib.txt")).unwrap(), "v1");
        assert!(cache_dir.join(".complete").exists());
        assert!(
            !cache_sibling_path(&cache_dir, &format!(".staging-{}", std::process::id())).exists(),
            "the staging directory becomes the cache"
        );
    }

    #[test]
    fn populate_cache_dir_once_leaves_a_complete_cache_alone() {
        let root = TempDir::new().unwrap();
        let cache_dir = root.path().join("abc123-1.0.0");
        populate_cache_dir_once(&cache_dir, |staging| write_file(staging, "lib.txt", "v1"))
            .unwrap();

        populate_cache_dir_once(&cache_dir, |_| {
            panic!("a complete cache must not be filled again")
        })
        .unwrap();

        assert_eq!(fs::read_to_string(cache_dir.join("lib.txt")).unwrap(), "v1");
    }

    /// A run interrupted before the marker leaves a partial cache behind; the
    /// next run replaces it whole instead of trusting any of it.
    #[test]
    fn populate_cache_dir_once_replaces_a_cache_left_without_its_marker() {
        let root = TempDir::new().unwrap();
        let cache_dir = root.path().join("abc123-1.0.0");
        write_file(&cache_dir, "stale.txt", "partial").unwrap();

        populate_cache_dir_once(&cache_dir, |staging| write_file(staging, "lib.txt", "v1"))
            .unwrap();

        assert!(!cache_dir.join("stale.txt").exists());
        assert_eq!(fs::read_to_string(cache_dir.join("lib.txt")).unwrap(), "v1");
        assert!(cache_dir.join(".complete").exists());
    }

    #[test]
    fn populate_cache_dir_once_marks_nothing_complete_when_the_fill_fails() {
        let root = TempDir::new().unwrap();
        let cache_dir = root.path().join("abc123-1.0.0");

        let result = populate_cache_dir_once(&cache_dir, |_| {
            Err(io::Error::new(ErrorKind::NotFound, "no embedded file").into())
        });

        assert!(result.is_err());
        assert!(!cache_dir.join(".complete").exists());
    }

    #[test]
    fn cache_sibling_path_preserves_semver_dots() {
        let cache_dir = PathBuf::from("/data/libs/rust/abc123def456-1.0.0");
        assert_eq!(
            cache_sibling_path(&cache_dir, ".lock"),
            PathBuf::from("/data/libs/rust/abc123def456-1.0.0.lock"),
        );
        assert_eq!(
            cache_sibling_path(&cache_dir, ".staging-42"),
            PathBuf::from("/data/libs/rust/abc123def456-1.0.0.staging-42"),
        );
    }

    #[test]
    fn cache_sibling_path_works_without_dots() {
        let cache_dir = PathBuf::from("/data/libs/rust/abc123def456");
        assert_eq!(
            cache_sibling_path(&cache_dir, ".lock"),
            PathBuf::from("/data/libs/rust/abc123def456.lock"),
        );
    }
}
