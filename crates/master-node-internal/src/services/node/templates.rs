use askama::Template;
use std::path::Path;

use crate::Result;

/// Embedded static file for Rust main.rs
const RUST_MAIN_RS: &str = include_str!("../../../templates/node_init/rust/src/main.rs");

/// Embedded static file for Python main.py
const PYTHON_MAIN_PY: &str = include_str!("../../../templates/node_init/python/src/main.py");

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

/// Applies templates and copies static files for Rust node initialization
pub fn apply_rust_templates(node_name: &str, node_dir: &Path) -> Result<()> {
    // Create src directory and write embedded main.rs
    let src_dir = node_dir.join("src");
    std::fs::create_dir_all(&src_dir)?;
    std::fs::write(src_dir.join("main.rs"), RUST_MAIN_RS)?;

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
    // Create src directory and write embedded main.py
    let src_dir = node_dir.join("src");
    std::fs::create_dir_all(&src_dir)?;
    std::fs::write(src_dir.join("main.py"), PYTHON_MAIN_PY)?;

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
