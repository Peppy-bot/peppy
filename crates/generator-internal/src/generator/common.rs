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
struct PeppyPythonConfigTemplate;

impl PeppyPythonConfigTemplate {
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
            let tpl = PeppyPythonConfigTemplate {};
            Ok(tpl.render()?)
        }
        _ => Err(Error::UnknownTemplate(template_path.to_string())),
    }
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
