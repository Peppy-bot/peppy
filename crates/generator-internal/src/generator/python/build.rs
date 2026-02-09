use crate::error::Result;
use crate::generator::common::{WorkspacePackageMetadata, copy_embedded_crate};
use crate::generator::types::{CapnpSchema, InterfaceArtifact, InterfaceKind};
use rust_embed::Embed;
use std::collections::{BTreeMap, HashMap};
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

    let capnp_dir = to_path.join("peppygen").join("capnp");
    fs::create_dir_all(&capnp_dir)?;
    for schema in schemas.values() {
        let file_path = capnp_dir.join(format!("{}.capnp", schema.file_stem()));
        fs::write(&file_path, schema.schema())?;
    }

    Ok(())
}

pub fn add_parameters_to_lib(parameters: &config::NodeArguments, to_path: &Path) -> Result<()> {
    let parameters_code = super::parameters::generate_python_parameters(parameters)?;
    let peppygen_dir = to_path.join("peppygen");
    fs::create_dir_all(&peppygen_dir)?;
    let parameters_file = peppygen_dir.join("parameters.py");
    fs::write(&parameters_file, parameters_code)?;
    Ok(())
}

pub fn add_artifacts_to_lib(to_path: &Path, artifacts: Vec<InterfaceArtifact>) -> Result<()> {
    let peppygen_dir = to_path.join("peppygen");

    let mut grouped: BTreeMap<ModuleCategory, Vec<InterfaceArtifact>> = BTreeMap::new();
    for artifact in artifacts {
        let category = ModuleCategory::from_kind(artifact.kind);
        grouped.entry(category).or_default().push(artifact);
    }

    for category in ModuleCategory::ALL {
        let category_dir = peppygen_dir.join(category.dir_name());
        if category_dir.exists() {
            fs::remove_dir_all(&category_dir)?;
        }
        fs::create_dir_all(&category_dir)?;

        let artifacts = grouped.remove(&category).unwrap_or_default();
        write_category(&category_dir, artifacts)?;
    }

    Ok(())
}

fn write_category(category_dir: &Path, artifacts: Vec<InterfaceArtifact>) -> Result<()> {
    let mut module_names: Vec<String> = Vec::new();
    let mut counts: HashMap<String, usize> = HashMap::new();

    for artifact in artifacts {
        let module_name = unique_module_name(&artifact.node_name, &mut counts);
        let module_file = category_dir.join(format!("{module_name}.py"));
        let mut code = artifact.code_output;
        if !code.ends_with('\n') {
            code.push('\n');
        }
        fs::write(&module_file, code)?;
        module_names.push(module_name);
    }

    let mut init_content = String::new();
    for name in &module_names {
        init_content.push_str(&format!("from . import {name}\n"));
    }
    fs::write(category_dir.join("__init__.py"), init_content)?;

    Ok(())
}

fn unique_module_name(original: &str, counts: &mut HashMap<String, usize>) -> String {
    let base = sanitize_module_name(original);
    let counter = counts.entry(base.clone()).or_insert(0);
    let name = if *counter == 0 {
        base.clone()
    } else {
        format!("{base}_{counter}")
    };
    *counter += 1;
    name
}

fn sanitize_module_name(raw: &str) -> String {
    let mut out = String::new();
    let mut last_was_underscore = false;

    for ch in raw.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            last_was_underscore = false;
        } else if !out.is_empty() && !last_was_underscore {
            out.push('_');
            last_was_underscore = true;
        } else if out.is_empty() {
            last_was_underscore = true;
        }
    }

    while out.ends_with('_') {
        out.pop();
    }

    if out.is_empty() {
        return "module".to_string();
    }

    if matches!(out.chars().next(), Some(ch) if ch.is_ascii_digit()) {
        out.insert(0, '_');
    }

    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ModuleCategory {
    ExposedTopics,
    SubscribedTopics,
    ExposedServices,
    SubscribedServices,
    ExposedActions,
    SubscribedActions,
}

impl ModuleCategory {
    const ALL: [Self; 6] = [
        Self::ExposedTopics,
        Self::SubscribedTopics,
        Self::ExposedServices,
        Self::SubscribedServices,
        Self::ExposedActions,
        Self::SubscribedActions,
    ];

    fn from_kind(kind: InterfaceKind) -> Self {
        match kind {
            InterfaceKind::ExposedTopic => Self::ExposedTopics,
            InterfaceKind::SubscribedTopic => Self::SubscribedTopics,
            InterfaceKind::ExposedService => Self::ExposedServices,
            InterfaceKind::SubscribedService => Self::SubscribedServices,
            InterfaceKind::ExposedAction => Self::ExposedActions,
            InterfaceKind::SubscribedAction => Self::SubscribedActions,
        }
    }

    fn dir_name(self) -> &'static str {
        match self {
            Self::ExposedTopics => "exposed_topics",
            Self::SubscribedTopics => "subscribed_topics",
            Self::ExposedServices => "exposed_services",
            Self::SubscribedServices => "subscribed_services",
            Self::ExposedActions => "exposed_actions",
            Self::SubscribedActions => "subscribed_actions",
        }
    }
}
