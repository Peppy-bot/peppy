use askama::Template;
use std::path::Path;

use crate::Result;

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

/// Applies templates and copies static files for Rust node initialization
pub fn apply_rust_templates(node_name: &str, node_dir: &Path) -> Result<()> {
    // Create src directory
    let src_dir = node_dir.join("src");
    std::fs::create_dir_all(&src_dir)?;

    // Copy static main.rs (no templating needed)
    let main_rs_content = include_str!("../../../templates/node_init/rust/src/main.rs");
    std::fs::write(src_dir.join("main.rs"), main_rs_content)?;

    // Apply Cargo.toml template
    let cargo_toml = RustCargoToml {
        node_name,
        pepygen_path: config::consts::PEPPYGEN_OUTPUT_PATH,
    };
    std::fs::write(node_dir.join("Cargo.toml"), cargo_toml.render()?)?;

    Ok(())
}

/// Applies templates and copies static files for Python node initialization
pub fn apply_python_templates(node_name: &str, node_dir: &Path) -> Result<()> {
    // Copy static main.py (no templating needed)
    let main_py_content = include_str!("../../../templates/node_init/python/main.py");
    std::fs::write(node_dir.join("main.py"), main_py_content)?;

    // Apply pyproject.toml template
    let pyproject_toml = PythonPyprojectToml { node_name };
    std::fs::write(node_dir.join("pyproject.toml"), pyproject_toml.render()?)?;

    Ok(())
}
