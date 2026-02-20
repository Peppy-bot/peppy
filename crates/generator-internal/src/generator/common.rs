use crate::error::{Error, Result};
use askama::Template;
use rust_embed::Embed;
use std::{
    fs,
    io::{self, ErrorKind},
    path::Path,
};
use toml_edit::{DocumentMut, value};

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
    python_min_version: &'a str,
    python_max_version: &'a str,
}

impl PeppyPythonConfigTemplate<'_> {
    pub const TEMPLATE_PATH: &'static str = "peppygen/python/pyproject.toml.j2";
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
// ---------------------------------------------------------------------------

#[derive(Embed)]
#[folder = "../peppylib/"]
#[include = "*.rs"]
#[include = "*.toml"]
#[include = "*.capnp"]
#[include = "*.j2"]
#[exclude = "target/*"]
#[exclude = "tests/*"]
#[exclude = "examples/*"]
pub(crate) struct EmbeddedPeppylib;

#[derive(Embed)]
#[folder = "../pmi-internal/"]
#[include = "*.rs"]
#[include = "*.toml"]
#[include = "*.capnp"]
#[include = "*.j2"]
#[exclude = "target/*"]
#[exclude = "tests/*"]
#[exclude = "examples/*"]
pub(crate) struct EmbeddedPmiInternal;

#[derive(Embed)]
#[folder = "../config-internal/"]
#[include = "*.rs"]
#[include = "*.toml"]
#[include = "*.capnp"]
#[include = "*.j2"]
#[include = "tools/capnp_*"]
#[exclude = "target/*"]
#[exclude = "tests/*"]
#[exclude = "examples/*"]
pub(crate) struct EmbeddedConfigInternal;

// ---------------------------------------------------------------------------
// Cross-platform symlink utility
// ---------------------------------------------------------------------------

pub(crate) fn symlink_dir(original: &Path, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    return std::os::unix::fs::symlink(original, link);
    #[cfg(windows)]
    return std::os::windows::fs::symlink_dir(original, link);
}

// ---------------------------------------------------------------------------
// Shared Rust crate cache deployment
// ---------------------------------------------------------------------------

/// Deploys the three vendored Rust crates (peppylib, pmi-internal, config-internal)
/// to a shared cache directory, then creates a symlink from `node_libs_dir/peppylib`
/// to the shared cache. This avoids duplicating source files across nodes.
///
/// The cache is keyed by content hash + version, and uses file locking with a
/// staging directory for concurrent-safe deployment.
pub(crate) fn deploy_rust_crates_to_shared_cache(node_libs_dir: &Path) -> Result<()> {
    let cache_key = format!("{}-{}", env!("RUST_CRATES_HASH"), env!("CARGO_PKG_VERSION"));
    let cache_dir = config::consts::peppy_data_dir()
        .join("libs/rust")
        .join(&cache_key);

    let parent = cache_dir
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "cache dir has no parent"))?;
    fs::create_dir_all(parent)?;
    let lock_path = cache_sibling_path(&cache_dir, ".lock");
    let lock_file = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    lock_file.lock()?;

    if !cache_dir.join(".complete").exists() {
        let staging_dir =
            cache_sibling_path(&cache_dir, &format!(".staging-{}", std::process::id()));
        if staging_dir.exists() {
            fs::remove_dir_all(&staging_dir)?;
        }

        let metadata = WorkspacePackageMetadata::embedded();
        copy_embedded_crate::<EmbeddedPeppylib>("peppylib", &staging_dir, &metadata)?;
        copy_embedded_crate::<EmbeddedPmiInternal>("pmi-internal", &staging_dir, &metadata)?;
        copy_embedded_crate::<EmbeddedConfigInternal>("config-internal", &staging_dir, &metadata)?;

        if cache_dir.exists() {
            fs::remove_dir_all(&cache_dir)?;
        }
        fs::rename(&staging_dir, &cache_dir)?;
        fs::write(cache_dir.join(".complete"), "")?;
    }
    drop(lock_file);

    // Create/replace symlinks for all three crates in node_libs_dir.
    // All three are needed because the crates reference each other via relative
    // sibling paths (e.g., peppylib has `config = { path = "../config-internal" }`),
    // and Cargo resolves these paths relative to the symlink location, not the target.
    for crate_name in &["peppylib", "pmi-internal", "config-internal"] {
        let link = node_libs_dir.join(crate_name);
        let target = cache_dir.join(crate_name);
        match link.symlink_metadata() {
            Ok(meta) if meta.file_type().is_symlink() => {
                if fs::read_link(&link).ok().as_deref() == Some(target.as_path()) {
                    continue;
                }
                fs::remove_file(&link)?;
            }
            Ok(_) => fs::remove_dir_all(&link)?,
            Err(_) => {}
        }
        symlink_dir(&target, &link)?;
    }

    Ok(())
}

/// Returns a sibling path of `cache_dir` by appending `suffix` to its full name.
///
/// Unlike `Path::with_extension`, this preserves dots in the original name.
/// For example, given `some/path/abc123-1.0.0` and suffix `.lock`, this returns
/// `some/path/abc123-1.0.0.lock` (not `some/path/abc123-1.0.lock`).
pub(crate) fn cache_sibling_path(cache_dir: &Path, suffix: &str) -> std::path::PathBuf {
    let name = cache_dir
        .file_name()
        .expect("cache_dir must have a file name component");
    let mut new_name = name.to_os_string();
    new_name.push(suffix);
    cache_dir.with_file_name(new_name)
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

/// Copies all files from an embedded crate into `vendored_root/crate_dir`,
/// then localizes the `Cargo.toml` to replace workspace inheritance.
pub(crate) fn copy_embedded_crate<E: Embed>(
    crate_dir: &str,
    vendored_root: &Path,
    metadata: &WorkspacePackageMetadata,
) -> Result<()> {
    let destination_dir = vendored_root.join(crate_dir);
    if destination_dir.exists() {
        fs::remove_dir_all(&destination_dir)?;
    }

    for file_path in E::iter() {
        let file_path_str = file_path.as_ref();
        let destination = destination_dir.join(file_path_str);

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }

        let content = E::get(file_path_str).ok_or_else(|| {
            io::Error::new(
                ErrorKind::NotFound,
                format!("embedded file not found: {file_path_str}"),
            )
        })?;
        fs::write(&destination, content.data.as_ref())?;

        // Set execute permissions on binary files in tools/ directory
        #[cfg(unix)]
        if file_path_str.starts_with("tools/") {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&destination)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&destination, perms)?;
        }
    }

    localize_cargo_toml(&destination_dir.join("Cargo.toml"), metadata)?;
    Ok(())
}

/// Replaces workspace-inherited fields (`version.workspace = true`, etc.)
/// in a `Cargo.toml` with concrete values from the given metadata.
pub(crate) fn localize_cargo_toml(
    cargo_toml_path: &Path,
    metadata: &WorkspacePackageMetadata,
) -> Result<()> {
    if !cargo_toml_path.exists() {
        return Ok(());
    }

    let contents = fs::read_to_string(cargo_toml_path)?;
    let mut doc: DocumentMut = contents
        .parse()
        .map_err(|err| io::Error::new(ErrorKind::InvalidData, err))?;

    if let Some(package) = doc.get_mut("package").and_then(|p| p.as_table_mut()) {
        if package
            .get("version")
            .and_then(|v| v.as_table())
            .and_then(|table| table.get("workspace"))
            .and_then(|w| w.as_bool())
            == Some(true)
        {
            package.insert("version", value(metadata.version));
        }

        if package
            .get("edition")
            .and_then(|v| v.as_table())
            .and_then(|table| table.get("workspace"))
            .and_then(|w| w.as_bool())
            == Some(true)
        {
            package.insert("edition", value(metadata.edition));
        }

        if package
            .get("authors")
            .and_then(|v| v.as_table())
            .and_then(|table| table.get("workspace"))
            .and_then(|w| w.as_bool())
            == Some(true)
        {
            package.remove("authors");
        }
    }

    fs::write(cargo_toml_path, doc.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
