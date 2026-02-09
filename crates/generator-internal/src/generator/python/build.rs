use crate::error::Result;
use crate::generator::common::{WorkspacePackageMetadata, copy_embedded_crate};
use crate::generator::types::CapnpSchema;
use rust_embed::Embed;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Embed)]
#[folder = "../peppylib-py/"]
#[include = "*.rs"]
#[include = "*.toml"]
#[include = "*.py"]
#[exclude = "target/*"]
#[exclude = "tests/*"]
#[exclude = "examples/*"]
#[exclude = "*.so"]
#[exclude = "*.lock"]
#[exclude = "__pycache__/*"]
struct EmbeddedPeppylibPy;

#[derive(Embed)]
#[folder = "../peppylib/"]
#[include = "*.rs"]
#[include = "*.toml"]
#[include = "*.capnp"]
#[include = "*.j2"]
#[exclude = "target/*"]
#[exclude = "tests/*"]
#[exclude = "examples/*"]
struct EmbeddedPeppylib;

#[derive(Embed)]
#[folder = "../pmi-internal/"]
#[include = "*.rs"]
#[include = "*.toml"]
#[include = "*.capnp"]
#[include = "*.j2"]
#[exclude = "target/*"]
#[exclude = "tests/*"]
struct EmbeddedPmiInternal;

#[derive(Embed)]
#[folder = "../config-internal/"]
#[include = "*.rs"]
#[include = "*.toml"]
#[include = "*.capnp"]
#[include = "*.j2"]
#[include = "tools/capnp_*"]
#[exclude = "target/*"]
#[exclude = "tests/*"]
struct EmbeddedConfigInternal;

#[derive(Embed)]
#[folder = "../node-stack-internal/"]
#[include = "*.rs"]
#[include = "*.toml"]
#[include = "*.capnp"]
#[include = "*.j2"]
#[exclude = "target/*"]
#[exclude = "tests/*"]
struct EmbeddedNodeStackInternal;

pub fn add_peppylib_dependencies(to_path: &Path) -> Result<()> {
    const PEPPYLIB_PY_DIR: &str = "peppylib-py";
    const PEPPYLIB_DIR: &str = "peppylib";
    const PMI_INTERNAL_DIR: &str = "pmi-internal";
    const CONFIG_INTERNAL_DIR: &str = "config-internal";
    const NODE_STACK_DIR: &str = "node-stack-internal";
    const VENDORED_ROOT: &str = "crates";
    const PEPPYLIB_PY_RELATIVE_PATH: &str = "crates/peppylib-py";

    let vendored_crates_dir = to_path.join(VENDORED_ROOT);
    fs::create_dir_all(&vendored_crates_dir)?;

    // Copy Python project templates (pyproject.toml, peppygen/__init__.py)
    crate::generator::common::copy_embedded_templates(
        "peppygen/python",
        to_path,
        PEPPYLIB_PY_RELATIVE_PATH,
    )?;

    let metadata = WorkspacePackageMetadata::embedded();

    // Vendor peppylib-py and its Rust dependency crates
    copy_embedded_crate::<EmbeddedPeppylibPy>(PEPPYLIB_PY_DIR, &vendored_crates_dir, &metadata)?;
    copy_embedded_crate::<EmbeddedPeppylib>(PEPPYLIB_DIR, &vendored_crates_dir, &metadata)?;
    copy_embedded_crate::<EmbeddedPmiInternal>(PMI_INTERNAL_DIR, &vendored_crates_dir, &metadata)?;
    copy_embedded_crate::<EmbeddedConfigInternal>(
        CONFIG_INTERNAL_DIR,
        &vendored_crates_dir,
        &metadata,
    )?;
    copy_embedded_crate::<EmbeddedNodeStackInternal>(
        NODE_STACK_DIR,
        &vendored_crates_dir,
        &metadata,
    )?;

    Ok(())
}

pub fn add_capnp_schemas(schemas: &HashMap<String, CapnpSchema>, to_path: &Path) -> Result<()> {
    if schemas.is_empty() {
        return Ok(());
    }

    let capnp_dir = to_path.join("capnp");
    std::fs::create_dir_all(&capnp_dir)?;
    for schema in schemas.values() {
        let file_path = capnp_dir.join(format!("{}.capnp", schema.file_stem()));
        std::fs::write(&file_path, schema.schema())?;
    }

    Ok(())
}

pub fn add_parameters_to_lib(parameters: &config::NodeArguments, to_path: &Path) -> Result<()> {
    let parameters_code = super::parameters::generate_python_parameters(parameters)?;
    let parameters_file = to_path.join("parameters.py");
    std::fs::write(&parameters_file, parameters_code)?;
    Ok(())
}
