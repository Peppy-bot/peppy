use askama::Template;
use std::path::Path;

use crate::Result;

/// Path to the templates directory
const TEMPLATES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/templates");

/// Template for Rust Cargo.toml file
#[derive(Template)]
#[template(path = "node_init/rust/Cargo.toml.j2")]
pub struct RustCargoToml<'a> {
    pub node_name: &'a str,
    pub pepygen_path: &'a str,
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

/// Recursively copies all non-template files (files without .j2 extension) from
/// the source directory to the destination directory, preserving the directory structure.
fn copy_static_files(src_dir: &Path, dest_dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src_dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = path.file_name().unwrap();
        let dest_path = dest_dir.join(file_name);

        if path.is_dir() {
            std::fs::create_dir_all(&dest_path)?;
            copy_static_files(&path, &dest_path)?;
        } else if path.extension().and_then(|e| e.to_str()) != Some("j2") {
            std::fs::copy(&path, &dest_path)?;
        }
    }
    Ok(())
}

/// Applies templates and copies static files for Rust node initialization
pub fn apply_rust_templates(node_name: &str, node_dir: &Path) -> Result<()> {
    let template_dir = Path::new(TEMPLATES_DIR).join("node_init/rust");

    // Copy all static files (non-.j2 files) recursively
    copy_static_files(&template_dir, node_dir)?;

    // Apply Cargo.toml template
    let cargo_toml = RustCargoToml {
        node_name,
        pepygen_path: config::consts::PEPPYGEN_OUTPUT_PATH,
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
    let template_dir = Path::new(TEMPLATES_DIR).join("node_init/python");

    // Copy all static files (non-.j2 files) recursively
    copy_static_files(&template_dir, node_dir)?;

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
