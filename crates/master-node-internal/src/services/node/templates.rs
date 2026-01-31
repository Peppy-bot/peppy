use askama::Template;
use rust_embed::Embed;
use std::path::Path;

use crate::Result;

#[derive(Embed)]
#[folder = "templates/"]
#[include = "*.rs"]
#[include = "*.py"]
#[exclude = "*.j2"]
struct EmbeddedTemplates;

/// Template for Rust Cargo.toml file
#[derive(Template)]
#[template(path = "node_init/rust/Cargo.toml.j2")]
pub struct RustCargoToml<'a> {
    pub node_name: &'a str,
    pub pepygen_path: &'a str,
    pub pepylib_path: &'a str,
}

/// Template for Python pyproject.toml file
#[derive(Template)]
#[template(path = "node_init/python/pyproject.toml.j2")]
pub struct PythonPyprojectToml<'a> {
    pub node_name: &'a str,
}

/// Template for Rust peppy.json5 file
#[derive(Template)]
#[template(path = "node_init/rust/peppy.json5.j2")]
pub struct RustPeppyJson5<'a> {
    pub node_name: &'a str,
}

/// Template for Python peppy.json5 file
#[derive(Template)]
#[template(path = "node_init/python/peppy.json5.j2")]
pub struct PythonPeppyJson5<'a> {
    pub node_name: &'a str,
}

/// Copies all embedded static files under the given prefix
/// to the destination directory, preserving the directory structure.
fn copy_embedded_static_files(prefix: &str, dest_dir: &Path) -> Result<()> {
    let prefix_with_slash = if prefix.ends_with('/') {
        prefix.to_string()
    } else {
        format!("{prefix}/")
    };

    for file_path in EmbeddedTemplates::iter() {
        let file_path_str = file_path.as_ref();

        let Some(relative_path) = file_path_str.strip_prefix(&prefix_with_slash) else {
            continue;
        };

        let dest_path = dest_dir.join(relative_path);

        if let Some(parent) = dest_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = EmbeddedTemplates::get(file_path_str).ok_or_else(|| {
            std::io::Error::other(format!("embedded file not found: {file_path_str}"))
        })?;
        std::fs::write(&dest_path, content.data.as_ref())?;
    }

    Ok(())
}

/// Applies templates and copies static files for Rust node initialization
pub fn apply_rust_templates(node_name: &str, node_dir: &Path) -> Result<()> {
    // Copy all static files (non-.j2 files) recursively
    copy_embedded_static_files("node_init/rust", node_dir)?;

    // Apply Cargo.toml template
    let cargo_toml = RustCargoToml {
        node_name,
        pepygen_path: config::consts::PEPPYGEN_OUTPUT_PATH,
        pepylib_path: config::consts::PEPPYLIB_OUTPUT_PATH,
    };
    std::fs::write(node_dir.join("Cargo.toml"), cargo_toml.render()?)?;

    // Apply peppy.json5 template
    let peppy_json5 = RustPeppyJson5 { node_name };
    std::fs::write(
        node_dir.join(config::consts::NODE_CONFIG_FILE),
        peppy_json5.render()?,
    )?;

    Ok(())
}

/// Applies templates and copies static files for Python node initialization
pub fn apply_python_templates(node_name: &str, node_dir: &Path) -> Result<()> {
    // Copy all static files (non-.j2 files) recursively
    copy_embedded_static_files("node_init/python", node_dir)?;

    // Apply pyproject.toml template
    let pyproject_toml = PythonPyprojectToml { node_name };
    std::fs::write(node_dir.join("pyproject.toml"), pyproject_toml.render()?)?;

    // Apply peppy.json5 template
    let peppy_json5 = PythonPeppyJson5 { node_name };
    std::fs::write(
        node_dir.join(config::consts::NODE_CONFIG_FILE),
        peppy_json5.render()?,
    )?;

    Ok(())
}
