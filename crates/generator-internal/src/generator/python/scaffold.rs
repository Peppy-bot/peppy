use super::identifiers::is_python_keyword;
use crate::error::Result;
use crate::generator::common::{cache_sibling_path, copy_dir_recursive};
use crate::generator::naming::{sanitize_component, unique_module_name};
#[cfg(test)]
use crate::generator::types::InterfaceKind;
use crate::generator::types::{CapnpSchema, InterfaceArtifact, ModuleCategory};
use rust_embed::Embed;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::Path;

/// Pre-built peppylib Python package (Python wrappers + compiled native extensions).
///
/// Contains platform-suffixed `.so` files (e.g. `_peppylib.abi3.macos-aarch64.so`,
/// `_peppylib.abi3.linux-aarch64.so`). During deployment, the correct platform's
/// `.so` is selected and renamed to `_peppylib.abi3.so`.
#[derive(Embed)]
#[folder = "../peppylib-py/peppylib/"]
#[include = "*.py"]
#[include = "*.so"]
#[exclude = "__pycache__/*"]
#[exclude = "_peppylib.abi3.so"]
struct EmbeddedPeppylibPy;

/// The filename prefix that all platform-suffixed native extensions share.
const SO_PLATFORM_PREFIX: &str = "_peppylib.abi3.";
/// The canonical filename Python uses to import the native extension.
const SO_CANONICAL_NAME: &str = "_peppylib.abi3.so";

/// Returns the platform suffix for the current host (e.g. "macos-aarch64", "linux-x86_64").
fn host_platform_suffix() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Returns the platform suffix to deploy.
///
/// - Non-container nodes run on the host, so use the host platform's `.so`.
/// - Container nodes run inside a Linux VM, so use the Linux `.so` matching
///   the host architecture.
fn target_platform_suffix(is_container: bool) -> String {
    if is_container {
        format!("linux-{}", std::env::consts::ARCH)
    } else {
        host_platform_suffix()
    }
}

/// Checks whether an embedded filename is a platform-suffixed `.so` file.
fn is_platform_so(filename: &str) -> bool {
    filename.starts_with(SO_PLATFORM_PREFIX)
        && filename.ends_with(".so")
        && filename != SO_CANONICAL_NAME
}

pub fn add_peppylib_dependencies(
    to_path: &Path,
    peppy_dirs: &config::consts::PeppyDirs,
    is_container: bool,
) -> Result<()> {
    // Copy Python project templates (pyproject.toml, peppygen/__init__.py)
    crate::generator::common::copy_embedded_templates("peppygen/python", to_path, "")?;

    // Determine which platform's .so to deploy, and include it in the cache key
    // so that native and container caches are separate.
    let target_suffix = target_platform_suffix(is_container);
    let expected_so_name = format!("{SO_PLATFORM_PREFIX}{target_suffix}.so");
    let cache_key = format!(
        "{}-{}-{}",
        env!("PEPPYLIB_SO_HASH"),
        env!("CARGO_PKG_VERSION"),
        target_suffix,
    );
    let cache_dir = peppy_dirs.python_libs_cache_dir(&cache_key);

    if !cache_dir.join(".complete").exists() {
        let parent = cache_dir.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "cache dir has no parent")
        })?;
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

            let mut found_target_so = false;
            for file_path in EmbeddedPeppylibPy::iter() {
                let file_path_str = file_path.as_ref();
                let filename = Path::new(file_path_str)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(file_path_str);

                // Skip platform .so files that don't match the target
                if is_platform_so(filename) {
                    if filename != expected_so_name {
                        continue;
                    }
                    // Rename the matching platform .so to the canonical name
                    let canonical_path = Path::new(file_path_str).with_file_name(SO_CANONICAL_NAME);
                    let destination = staging_dir.join(canonical_path);
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
                    found_target_so = true;
                    continue;
                }

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

            if !found_target_so {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "no embedded native extension found for platform '{target_suffix}' \
                         (expected '{expected_so_name}')"
                    ),
                )
                .into());
            }

            if cache_dir.exists() {
                fs::remove_dir_all(&cache_dir)?;
            }
            fs::rename(&staging_dir, &cache_dir)?;
            fs::write(cache_dir.join(".complete"), "")?;
        }
        drop(lock_file);
    }

    // Always copy peppylib into the output directory so each node gets the
    // correct platform binary (host or Linux) without shared symlinks.
    let peppylib_dest = to_path.join("peppylib");
    if peppylib_dest.exists() {
        fs::remove_dir_all(&peppylib_dest)?;
    }
    copy_dir_recursive(&cache_dir, &peppylib_dest)?;

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

pub fn add_parameters_to_lib(parameters: &config::ParameterSchema, to_path: &Path) -> Result<()> {
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
        let tree = build_module_tree(artifacts);
        write_tree_node(&category_dir, &tree)?;
    }

    Ok(())
}

#[derive(Default)]
struct ModuleTree {
    children: BTreeMap<String, ModuleTree>,
    leaves: BTreeMap<String, Vec<InterfaceArtifact>>,
}

fn build_module_tree(artifacts: Vec<InterfaceArtifact>) -> ModuleTree {
    let mut root = ModuleTree::default();
    for artifact in artifacts {
        let path = artifact.module_path.clone();
        insert_into_tree(&mut root, &path, artifact);
    }
    root
}

fn insert_into_tree(node: &mut ModuleTree, path: &[String], artifact: InterfaceArtifact) {
    match path {
        [] => unreachable!("InterfaceArtifact::module_path must not be empty"),
        [leaf] => {
            node.leaves.entry(leaf.clone()).or_default().push(artifact);
        }
        [segment, rest @ ..] => {
            let child = node.children.entry(segment.clone()).or_default();
            insert_into_tree(child, rest, artifact);
        }
    }
}

/// Recursive worker mirroring the Rust scaffold layout. Writes one `.py` per
/// leaf and an `__init__.py` per directory that imports every sub-module so
/// `from peppygen.emitted_topics.depth_camera.v1 import video_stream` resolves.
fn write_tree_node(dir: &Path, tree: &ModuleTree) -> Result<()> {
    fs::create_dir_all(dir)?;

    let mut init_imports: Vec<String> = Vec::new();
    let mut counts: HashMap<String, usize> = HashMap::new();

    for (raw_leaf, artifacts) in &tree.leaves {
        let module_name = unique_module_name(raw_leaf, &mut counts, sanitize_python_module_name);
        let module_file = dir.join(format!("{module_name}.py"));
        let mut code = String::new();
        for artifact in artifacts {
            if !code.is_empty() && !code.ends_with('\n') {
                code.push('\n');
            }
            code.push_str(&artifact.code_output);
        }
        if !code.ends_with('\n') {
            code.push('\n');
        }
        fs::write(&module_file, code)?;
        init_imports.push(module_name);
    }

    for (raw_segment, child) in &tree.children {
        let module_name = unique_module_name(raw_segment, &mut counts, sanitize_python_module_name);
        let child_dir = dir.join(&module_name);
        write_tree_node(&child_dir, child)?;
        init_imports.push(module_name);
    }

    let mut init_content = String::new();
    for name in &init_imports {
        init_content.push_str(&format!("from . import {name}\n"));
    }
    fs::write(dir.join("__init__.py"), init_content)?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn sanitize_module_name_escapes_python_keywords() {
        assert_eq!(sanitize_python_module_name("class"), "class_");
        assert_eq!(sanitize_python_module_name("from"), "from_");
    }

    #[test]
    fn write_tree_node_escapes_keyword_module_in_init_import() {
        let temp_dir = TempDir::new().expect("temp dir should be created");
        let artifact = InterfaceArtifact::from_kind(
            "class",
            InterfaceKind::EmittedTopic,
            String::from("x = 1\n"),
        );

        let tree = build_module_tree(vec![artifact]);
        write_tree_node(temp_dir.path(), &tree).expect("tree should be written");

        let module_file = temp_dir.path().join("class_.py");
        assert!(module_file.exists(), "expected escaped module filename");

        let init_content = fs::read_to_string(temp_dir.path().join("__init__.py"))
            .expect("expected __init__.py content");
        assert_eq!(init_content, "from . import class_\n");
    }

    #[test]
    fn write_tree_node_nests_conformed_artifacts() {
        let temp_dir = TempDir::new().expect("temp dir should be created");
        let native = InterfaceArtifact::from_kind(
            "video_stream",
            InterfaceKind::EmittedTopic,
            String::from("NATIVE = True\n"),
        );
        let conformed = InterfaceArtifact::from_kind_nested(
            vec![
                "depth_camera".to_string(),
                "v1".to_string(),
                "video_stream".to_string(),
            ],
            InterfaceKind::EmittedTopic,
            String::from("CONFORMED = True\n"),
        );

        let tree = build_module_tree(vec![native, conformed]);
        write_tree_node(temp_dir.path(), &tree).expect("tree should be written");

        assert!(temp_dir.path().join("video_stream.py").exists());
        assert!(
            temp_dir
                .path()
                .join("depth_camera/v1/video_stream.py")
                .exists()
        );
        let conformed_code =
            fs::read_to_string(temp_dir.path().join("depth_camera/v1/video_stream.py"))
                .expect("conformed file should be readable");
        assert!(conformed_code.contains("CONFORMED"));
        let root_init = fs::read_to_string(temp_dir.path().join("__init__.py")).unwrap();
        assert!(root_init.contains("from . import video_stream"));
        assert!(root_init.contains("from . import depth_camera"));
        let depth_init = fs::read_to_string(temp_dir.path().join("depth_camera/__init__.py"))
            .expect("depth_camera __init__.py should exist");
        assert_eq!(depth_init, "from . import v1\n");
    }

    #[test]
    fn embedded_peppylib_contains_platform_suffixed_so_files() {
        let so_files: Vec<String> = EmbeddedPeppylibPy::iter()
            .filter(|f| f.as_ref().ends_with(".so"))
            .map(|f| f.as_ref().to_string())
            .collect();

        assert!(
            !so_files.is_empty(),
            "expected at least one .so file in embedded peppylib"
        );

        // Every .so file should be platform-suffixed, not the canonical name
        for so_file in &so_files {
            let filename = Path::new(so_file).file_name().unwrap().to_str().unwrap();
            assert!(
                is_platform_so(filename),
                "expected platform-suffixed .so, got: {filename}"
            );
        }

        // The host platform's .so must be present
        let host_suffix = host_platform_suffix();
        let host_so = format!("{SO_PLATFORM_PREFIX}{host_suffix}.so");
        assert!(
            so_files.iter().any(|f| f.ends_with(&host_so)),
            "missing host platform .so ({host_so}), found: {so_files:?}"
        );
    }

    /// All release platforms must have their `.so` embedded. This test catches
    /// regressions where a new target is added to the release pipeline but the
    /// cross-compilation step is missing or broken. Only enforced on macOS where
    /// all release builds (including cross-compilation) originate.
    #[test]
    #[cfg(target_os = "macos")]
    fn embedded_peppylib_contains_all_release_platform_dynamic_lib() {
        let required = [
            "_peppylib.abi3.macos-aarch64.so",
            "_peppylib.abi3.linux-aarch64.so",
            "_peppylib.abi3.linux-x86_64.so",
        ];

        let embedded: Vec<String> = EmbeddedPeppylibPy::iter()
            .filter(|f| f.as_ref().ends_with(".so"))
            .map(|f| f.as_ref().to_string())
            .collect();

        for expected in &required {
            assert!(
                embedded.iter().any(|f| f.ends_with(expected)),
                "missing required .so: {expected}, found: {embedded:?}"
            );
        }
    }

    #[test]
    fn is_platform_so_identifies_suffixed_files() {
        assert!(is_platform_so("_peppylib.abi3.macos-aarch64.so"));
        assert!(is_platform_so("_peppylib.abi3.linux-aarch64.so"));
        assert!(is_platform_so("_peppylib.abi3.linux-x86_64.so"));
        assert!(
            !is_platform_so("_peppylib.abi3.so"),
            "canonical name should not be treated as platform-suffixed"
        );
        assert!(!is_platform_so("__init__.py"));
    }

    #[test]
    fn target_platform_suffix_selects_host_for_non_container() {
        let suffix = target_platform_suffix(false);
        assert_eq!(suffix, host_platform_suffix());
    }

    #[test]
    fn target_platform_suffix_selects_linux_for_container() {
        let suffix = target_platform_suffix(true);
        assert!(
            suffix.starts_with("linux-"),
            "container should select linux platform, got: {suffix}"
        );
        assert!(
            suffix.ends_with(std::env::consts::ARCH),
            "container should use host architecture, got: {suffix}"
        );
    }

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
