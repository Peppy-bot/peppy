use crate::error::{Error, Result};
use askama::Template;
use rust_embed::Embed;
use std::{
    collections::HashMap,
    fs,
    io::{self, ErrorKind},
    path::Path,
};

/// Builds the per-`(producer_name, producer_tag)` map of pinned sibling
/// link_ids declared in a node's `depends_on.{nodes, interfaces}`. The map
/// is keyed by `(name, tag)`; the value is the list of link_ids declared
/// with `from_any: false` (or unset) on that group. Used by codegen to
/// emit a single `MessengerHandle::register_consumer_dependencies` call at
/// node bootstrap so from_any consumers know which producer link_ids a
/// sibling pinned dependency on the same `(name, tag)` already claims.
///
/// Groups without any from_any sibling are still included; the registration
/// is a no-op when nothing wildcards on this `(name, tag)`. Filtering at
/// codegen time would require tracking the from_any/pinned split twice, so
/// the runtime lookup just returns an empty exclusion set in that case.
pub(crate) fn pinned_siblings_per_group(
    manifest: &config::node::Manifest,
) -> HashMap<(String, String), Vec<String>> {
    let mut out: HashMap<(String, String), Vec<String>> = HashMap::new();
    let Some(depends_on) = manifest.depends_on.as_ref() else {
        return out;
    };
    for node in &depends_on.nodes {
        if node.from_any {
            continue;
        }
        out.entry((node.name.as_str().to_string(), node.tag.clone()))
            .or_default()
            .push(node.link_id.clone());
    }
    for iface in &depends_on.interfaces {
        if iface.from_any {
            continue;
        }
        out.entry((iface.name.as_str().to_string(), iface.tag.clone()))
            .or_default()
            .push(iface.link_id.clone());
    }
    // Sort entries within each group so the generated code is
    // deterministic; important for codegen caching / fingerprints.
    for ids in out.values_mut() {
        ids.sort();
        ids.dedup();
    }
    out
}

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

#[derive(Embed)]
#[folder = "../build-helpers-internal/"]
#[include = "*.rs"]
#[include = "*.toml"]
#[exclude = "target/*"]
pub(crate) struct EmbeddedBuildHelpers;

#[derive(Embed)]
#[folder = "../core-node-api/"]
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
pub fn cache_sibling_path(cache_dir: &Path, suffix: &str) -> std::path::PathBuf {
    let name = cache_dir
        .file_name()
        .expect("cache_dir must have a file name component");
    let mut new_name = name.to_os_string();
    new_name.push(suffix);
    cache_dir.with_file_name(new_name)
}

// ---------------------------------------------------------------------------
// Crate deployment mode
// ---------------------------------------------------------------------------

/// Controls how vendored crates are linked into a node's `.peppy/libs/` directory.
///
/// `Symlink` (the default) creates symlinks to a shared cache — fast and avoids
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
