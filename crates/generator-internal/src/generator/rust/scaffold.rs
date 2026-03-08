use super::identifiers::is_rust_keyword;
use crate::generator::common::{
    CrateDeployMode, EmbeddedBuildHelpers, EmbeddedConfigInternal, EmbeddedPeppylib,
    EmbeddedPmiInternal, WorkspacePackageMetadata, cache_sibling_path, copy_dir_recursive,
};
use crate::{
    error::{Error, Result},
    generator::{
        naming::{sanitize_component, unique_module_name},
        types::{CapnpSchema, InterfaceArtifact, InterfaceKind},
    },
};
use config::encoding::compile_capnp;
use proc_macro2::Span;
use rust_embed::Embed;
use std::io;
use std::io::ErrorKind;
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
};
use syn::{
    Attribute, File, FnArg, ImplItem, ImplItemFn, Item, ItemFn, ItemImpl, Type, parse_file,
    parse_quote,
};
use toml_edit::{DocumentMut, value};

pub fn add_peppylib_dependencies(
    to_path: impl AsRef<Path>,
    peppy_dirs: &config::consts::PeppyDirs,
    deploy_mode: CrateDeployMode,
) -> Result<()> {
    let to_path = to_path.as_ref();
    let libs_dir = to_path.parent().ok_or_else(|| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "peppygen path has no parent directory",
        ))
    })?;

    deploy_rust_crates_to_shared_cache(libs_dir, peppy_dirs, deploy_mode)?;
    generate_lib_structure(to_path, "../peppylib")?;

    Ok(())
}

pub fn add_artifacts_to_lib(
    lib_path: impl AsRef<Path>,
    artifacts: Vec<InterfaceArtifact>,
) -> Result<()> {
    let lib_path = lib_path.as_ref();
    let src_dir = lib_path.join("src");
    fs::create_dir_all(&src_dir)?;

    let mut grouped = group_artifacts_by_category(artifacts);

    for category in ModuleCategory::ALL {
        let category_dir = prepare_category_dir(&src_dir, category)?;
        let category_file = category_module_path(&src_dir, category);
        let nodes = grouped.remove(&category).unwrap_or_default();
        write_category_modules(&category_dir, &category_file, nodes, category)?;
    }

    Ok(())
}

/// Generates and writes the parameters struct to the library.
///
/// This function takes the node's parameters configuration and generates a Rust struct
/// that can be used to deserialize parameter values at runtime.
pub fn add_parameters_to_lib(
    lib_path: impl AsRef<Path>,
    parameters: &config::NodeArguments,
) -> Result<()> {
    let lib_path = lib_path.as_ref();
    let src_dir = lib_path.join("src");
    fs::create_dir_all(&src_dir)?;

    let parameters_file = src_dir.join("parameters.rs");
    let code = super::generate_parameters_struct(parameters)?;
    fs::write(&parameters_file, code)?;

    Ok(())
}

pub fn add_capnp_schemas(schemas: &HashMap<String, CapnpSchema>, crate_root: &Path) -> Result<()> {
    let src_dir = crate_root.join("src");
    if schemas.is_empty() {
        let capnp_rs = src_dir.join("capnp.rs");
        if !capnp_rs.exists() {
            fs::write(capnp_rs, "// No Cap'n Proto schemas generated.\n")?;
        }
        return Ok(());
    }

    let mut entries: Vec<&CapnpSchema> = schemas.values().collect();
    entries.sort_by(|a, b| a.file_stem().cmp(b.file_stem()));

    let capnp_dir = src_dir.join("capnp");
    fs::create_dir_all(&capnp_dir)?;

    let mut schema_paths = Vec::with_capacity(entries.len());
    for schema in entries {
        let file_path = capnp_dir.join(format!("{}.capnp", schema.file_stem()));
        fs::write(&file_path, schema.schema())?;
        schema_paths.push(file_path);
    }

    compile_capnp(&schema_paths, &src_dir).map_err(Error::MessageEncoding)?;
    Ok(())
}

fn symlink_dir(original: &Path, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    return std::os::unix::fs::symlink(original, link);
}

/// Replaces workspace-inherited fields (`version.workspace = true`, etc.)
/// in a `Cargo.toml` with concrete values from the given metadata.
fn localize_cargo_toml(cargo_toml_path: &Path, metadata: &WorkspacePackageMetadata) -> Result<()> {
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

/// Copies all files from an embedded crate into `vendored_root/crate_dir`,
/// then localizes the `Cargo.toml` to replace workspace inheritance.
fn copy_embedded_crate<E: Embed>(
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

/// Deploys the four vendored Rust crates (peppylib, pmi-internal, config-internal, build-helpers-internal)
/// to a shared cache directory, then links or copies them into `node_libs_dir`.
///
/// In `Symlink` mode (the default), creates symlinks from `node_libs_dir/{crate}`
/// to the shared cache. This avoids duplicating source files across nodes.
///
/// In `Copy` mode, copies the crate sources directly into `node_libs_dir`.
/// This is needed for container builds where symlinks to host paths would break.
///
/// The cache is keyed by content hash + version, and uses file locking with a
/// staging directory for concurrent-safe deployment.
fn deploy_rust_crates_to_shared_cache(
    node_libs_dir: &Path,
    peppy_dirs: &config::consts::PeppyDirs,
    deploy_mode: CrateDeployMode,
) -> Result<()> {
    let cache_key = format!("{}-{}", env!("RUST_CRATES_HASH"), env!("CARGO_PKG_VERSION"));
    let cache_dir = peppy_dirs.rust_libs_cache_dir(&cache_key);

    let parent = cache_dir
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "cache dir has no parent"))?;
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

        let metadata = WorkspacePackageMetadata::embedded();
        copy_embedded_crate::<EmbeddedPeppylib>("peppylib", &staging_dir, &metadata)?;
        copy_embedded_crate::<EmbeddedPmiInternal>("pmi-internal", &staging_dir, &metadata)?;
        copy_embedded_crate::<EmbeddedConfigInternal>("config-internal", &staging_dir, &metadata)?;
        copy_embedded_crate::<EmbeddedBuildHelpers>(
            "build-helpers-internal",
            &staging_dir,
            &metadata,
        )?;

        if cache_dir.exists() {
            fs::remove_dir_all(&cache_dir)?;
        }
        fs::rename(&staging_dir, &cache_dir)?;
        fs::write(cache_dir.join(".complete"), "")?;
    }
    drop(lock_file);

    // Link or copy all four crates into node_libs_dir.
    // All four are needed because the crates reference each other via relative
    // sibling paths (e.g., peppylib has `config = { path = "../config-internal" }`
    // and `build-helpers = { path = "../build-helpers-internal" }` in build-dependencies),
    // and Cargo resolves these paths relative to the symlink location, not the target.
    for crate_name in &[
        "peppylib",
        "pmi-internal",
        "config-internal",
        "build-helpers-internal",
    ] {
        let dest = node_libs_dir.join(crate_name);
        let source = cache_dir.join(crate_name);

        match deploy_mode {
            CrateDeployMode::Symlink => {
                match dest.symlink_metadata() {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        if fs::read_link(&dest).ok().as_deref() == Some(source.as_path()) {
                            continue;
                        }
                        fs::remove_file(&dest)?;
                    }
                    Ok(_) => fs::remove_dir_all(&dest)?,
                    Err(_) => {}
                }
                symlink_dir(&source, &dest)?;
            }
            CrateDeployMode::Copy => {
                if dest.exists() {
                    fs::remove_dir_all(&dest)?;
                }
                copy_dir_recursive(&source, &dest)?;
            }
        }
    }

    Ok(())
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

    fn struct_name(self) -> &'static str {
        match self {
            Self::ExposedTopics => "ExposedTopics",
            Self::SubscribedTopics => "SubscribedTopics",
            Self::ExposedServices => "ExposedServices",
            Self::SubscribedServices => "SubscribedServices",
            Self::ExposedActions => "ExposedActions",
            Self::SubscribedActions => "SubscribedActions",
        }
    }

    fn module_file_name(self) -> &'static str {
        match self {
            Self::ExposedTopics => "exposed_topics",
            Self::SubscribedTopics => "subscribed_topics",
            Self::ExposedServices => "exposed_services",
            Self::SubscribedServices => "subscribed_services",
            Self::ExposedActions => "exposed_actions",
            Self::SubscribedActions => "subscribed_actions",
        }
    }

    fn doc_label(self) -> &'static str {
        match self {
            Self::ExposedTopics => "exposed topics",
            Self::SubscribedTopics => "subscribed topics",
            Self::ExposedServices => "exposed services",
            Self::SubscribedServices => "subscribed services",
            Self::ExposedActions => "exposed actions",
            Self::SubscribedActions => "subscribed actions",
        }
    }
}

fn generate_lib_structure(to_path: impl AsRef<Path>, peppylib_path: &str) -> Result<()> {
    let to_path = to_path.as_ref();
    fs::create_dir_all(to_path)?;

    crate::generator::common::copy_embedded_templates("peppygen/rust", to_path, peppylib_path)
}

fn group_artifacts_by_category(
    artifacts: Vec<InterfaceArtifact>,
) -> BTreeMap<ModuleCategory, BTreeMap<String, Vec<InterfaceArtifact>>> {
    let mut grouped: BTreeMap<ModuleCategory, BTreeMap<String, Vec<InterfaceArtifact>>> =
        BTreeMap::new();

    for artifact in artifacts {
        let category = ModuleCategory::from_kind(artifact.kind);
        grouped
            .entry(category)
            .or_default()
            .entry(artifact.node_name.clone())
            .or_default()
            .push(artifact);
    }

    grouped
}

fn prepare_category_dir(src_dir: &Path, category: ModuleCategory) -> Result<PathBuf> {
    let category_dir = src_dir.join(category.module_file_name());
    if category_dir.exists() {
        fs::remove_dir_all(&category_dir)?;
    }
    fs::create_dir_all(&category_dir)?;

    let category_file = category_module_path(src_dir, category);
    if category_file.exists() {
        fs::remove_file(&category_file)?;
    }

    Ok(category_dir)
}

fn category_module_path(src_dir: &Path, category: ModuleCategory) -> PathBuf {
    src_dir.join(format!("{}.rs", category.module_file_name()))
}

fn write_category_modules(
    category_dir: &Path,
    category_file: &Path,
    nodes: BTreeMap<String, Vec<InterfaceArtifact>>,
    category: ModuleCategory,
) -> Result<()> {
    let mut modules = Vec::new();
    let mut counts: HashMap<String, usize> = HashMap::new();

    for (original_name, artifacts) in nodes {
        let module_name =
            unique_module_name(&original_name, &mut counts, sanitize_rust_module_name);
        write_node_module(
            category_dir,
            &module_name,
            &original_name,
            artifacts,
            category,
        )?;
        modules.push((module_name, original_name));
    }

    write_category_module_file(category_file, &modules)?;
    Ok(())
}

fn write_category_module_file(category_file: &Path, modules: &[(String, String)]) -> Result<()> {
    let mut mod_section = String::new();
    for (module, original) in modules {
        if !original.is_empty() {
            mod_section.push_str(&format!("// Node: {original}\n"));
        }
        mod_section.push_str(&format!("pub mod {module};\n"));
    }

    let content = mod_section;

    fs::write(category_file, content)?;
    Ok(())
}

fn write_node_module(
    base_dir: &Path,
    module_name: &str,
    original_name: &str,
    artifacts: Vec<InterfaceArtifact>,
    category: ModuleCategory,
) -> Result<()> {
    let mut items: Vec<Item> = Vec::new();
    let struct_ident = syn::Ident::new(category.struct_name(), Span::call_site());
    let node_label = if original_name.is_empty() {
        module_name.to_string()
    } else {
        original_name.to_string()
    };
    let mut submodule_counts: HashMap<String, usize> = HashMap::new();
    let module_dir = base_dir.join(module_name);

    for artifact in artifacts {
        if let Some(submodule) = &artifact.submodule {
            let submodule_name =
                unique_module_name(submodule, &mut submodule_counts, sanitize_rust_module_name);
            fs::create_dir_all(&module_dir)?;
            let submodule_path = module_dir.join(format!("{submodule_name}.rs"));
            let mut contents = artifact.code_output;
            if !contents.ends_with('\n') {
                contents.push('\n');
            }
            fs::write(&submodule_path, contents)?;
            let ident = syn::Ident::new(&submodule_name, Span::call_site());
            let mod_item: Item = parse_quote!(pub mod #ident;);
            items.push(mod_item);
            continue;
        }

        let file =
            parse_file(&artifact.code_output).map_err(|source| Error::NodeModuleParseError {
                node: node_label.clone(),
                source,
            })?;

        for item in file.items {
            match item {
                Item::Impl(impl_block) if impl_targets_struct(&impl_block, &struct_ident) => {
                    items.extend(extract_impl_functions(impl_block));
                }
                other => items.push(other),
            }
        }
    }

    let mut attrs: Vec<Attribute> = Vec::new();
    if !original_name.is_empty() {
        let doc_comment = format!(
            "Generated {} interfaces for `{}`.",
            category.doc_label(),
            original_name
        );
        attrs.push(parse_quote!(#![doc = #doc_comment]));
    }
    let module_file = File {
        shebang: None,
        attrs,
        items,
    };

    let rendered = render_module_file(module_file);

    fs::write(base_dir.join(format!("{module_name}.rs")), rendered)?;
    Ok(())
}

fn render_module_file(module_file: File) -> String {
    let mut output = String::new();

    if let Some(shebang) = module_file.shebang {
        output.push_str(&shebang);
        output.push('\n');
    }

    if !module_file.attrs.is_empty() {
        let attr_file = File {
            shebang: None,
            attrs: module_file.attrs,
            items: Vec::new(),
        };
        let rendered_attrs = prettyplease::unparse(&attr_file);
        output.push_str(rendered_attrs.trim_end_matches('\n'));
        output.push('\n');
    }

    let mut items_iter = module_file.items.into_iter().peekable();
    while let Some(item) = items_iter.next() {
        let item_file = File {
            shebang: None,
            attrs: Vec::new(),
            items: vec![item],
        };
        let rendered_item = prettyplease::unparse(&item_file);
        output.push_str(rendered_item.trim_end_matches('\n'));
        output.push('\n');

        if items_iter.peek().is_some() {
            output.push('\n');
        }
    }

    if !output.ends_with('\n') {
        output.push('\n');
    }

    output
}

fn impl_targets_struct(impl_block: &ItemImpl, struct_ident: &syn::Ident) -> bool {
    let type_path = match impl_block.self_ty.as_ref() {
        Type::Path(path) if path.qself.is_none() => path,
        _ => return false,
    };

    let Some(last_segment) = type_path.path.segments.last() else {
        return false;
    };

    last_segment.ident == *struct_ident
}

fn extract_impl_functions(impl_block: ItemImpl) -> Vec<Item> {
    impl_block
        .items
        .into_iter()
        .filter_map(as_function_item)
        .collect()
}

fn as_function_item(item: ImplItem) -> Option<Item> {
    let ImplItem::Fn(method) = item else {
        return None;
    };

    if method
        .sig
        .inputs
        .iter()
        .any(|arg| matches!(arg, FnArg::Receiver(_)))
    {
        return None;
    }

    let ImplItemFn {
        attrs,
        vis,
        sig,
        block,
        ..
    } = method;

    Some(Item::Fn(ItemFn {
        attrs,
        vis,
        sig,
        block: Box::new(block),
    }))
}

fn sanitize_rust_module_name(raw: &str) -> String {
    let mut out = sanitize_component(raw);
    if out.is_empty() {
        return "node".to_string();
    }
    if is_rust_keyword(&out) {
        out.push('_');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_module_name_escapes_rust_keywords() {
        assert_eq!(sanitize_rust_module_name("mod"), "mod_");
        assert_eq!(sanitize_rust_module_name("type"), "type_");
    }
}
