use super::identifiers::is_python_keyword;
use crate::error::Result;
use crate::generator::naming::{sanitize_component, unique_module_name};
use crate::generator::types::{CapnpSchema, InterfaceArtifact, InterfaceKind};
use rust_embed::Embed;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::Path;

/// Pre-built peppylib Python package (Python wrappers + compiled native extension).
#[derive(Embed)]
#[folder = "../peppylib-py/peppylib/"]
#[include = "*.py"]
#[include = "*.so"]
#[exclude = "__pycache__/*"]
struct EmbeddedPeppylibPy;

pub fn add_peppylib_dependencies(to_path: &Path) -> Result<()> {
    // Copy Python project templates (pyproject.toml, peppygen/__init__.py)
    crate::generator::common::copy_embedded_templates("peppygen/python", to_path, "")?;

    // Deploy the pre-built peppylib Python package to a shared cache
    let cache_key = format!(
        "{}-{}",
        env!("PEPPYLIB_SO_MTIME"),
        env!("CARGO_PKG_VERSION")
    );
    let cache_dir = config::consts::peppy_data_dir()
        .join("libs/python")
        .join(&cache_key);

    if !cache_dir.join(".complete").exists() {
        fs::create_dir_all(cache_dir.parent().unwrap())?;
        let lock_path = cache_dir.with_extension("lock");
        let lock_file = fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        lock_file.lock()?;

        if !cache_dir.join(".complete").exists() {
            let staging_dir = cache_dir.with_extension(format!("staging-{}", std::process::id()));
            if staging_dir.exists() {
                fs::remove_dir_all(&staging_dir)?;
            }

            for file_path in EmbeddedPeppylibPy::iter() {
                let file_path_str = file_path.as_ref();
                let destination = staging_dir.join(file_path_str);
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                let content = EmbeddedPeppylibPy::get(file_path_str).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("embedded peppylib file not found: {file_path_str}"),
                    )
                })?;
                fs::write(&destination, content.data.as_ref())?;
            }

            if cache_dir.exists() {
                fs::remove_dir_all(&cache_dir)?;
            }
            fs::rename(&staging_dir, &cache_dir)?;
            fs::write(cache_dir.join(".complete"), "")?;
        }
    }

    // Symlink to_path/peppylib → shared cache
    let peppylib_link = to_path.join("peppylib");
    match peppylib_link.symlink_metadata() {
        Ok(meta) if meta.file_type().is_symlink() => fs::remove_file(&peppylib_link)?,
        Ok(_) => fs::remove_dir_all(&peppylib_link)?,
        Err(_) => {}
    }
    symlink_dir(&cache_dir, &peppylib_link)?;

    Ok(())
}

fn symlink_dir(original: &Path, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    return std::os::unix::fs::symlink(original, link);
    #[cfg(windows)]
    return std::os::windows::fs::symlink_dir(original, link);
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
        let module_name = unique_module_name(
            &artifact.node_name,
            &mut counts,
            sanitize_python_module_name,
        );
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

fn sanitize_python_module_name(raw: &str) -> String {
    let mut out = sanitize_component(raw);
    if out.is_empty() {
        return "module".to_string();
    }
    if is_python_keyword(&out) {
        out.push('_');
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn sanitize_module_name_escapes_python_keywords() {
        assert_eq!(sanitize_python_module_name("class"), "class_");
        assert_eq!(sanitize_python_module_name("from"), "from_");
    }

    #[test]
    fn write_category_escapes_keyword_module_in_init_import() {
        let temp_dir = TempDir::new().expect("temp dir should be created");
        let artifact = InterfaceArtifact::from_kind(
            "class",
            InterfaceKind::ExposedTopic,
            String::from("x = 1\n"),
        );

        write_category(temp_dir.path(), vec![artifact]).expect("category should be written");

        let module_file = temp_dir.path().join("class_.py");
        assert!(module_file.exists(), "expected escaped module filename");

        let init_content = fs::read_to_string(temp_dir.path().join("__init__.py"))
            .expect("expected __init__.py content");
        assert_eq!(init_content, "from . import class_\n");
    }
}
