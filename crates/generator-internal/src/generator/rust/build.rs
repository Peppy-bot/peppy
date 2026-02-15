use super::identifiers::is_rust_keyword;
use crate::{
    error::{Error, Result},
    generator::{
        common::stage_runtime_bundle,
        naming::{sanitize_component, unique_module_name},
        types::{CapnpSchema, InterfaceArtifact, InterfaceKind},
    },
};
use config::consts::{PEPPY_OUTPUT_DIR, PEPPYGEN_OUTPUT_PATH};
use config::encoding::compile_capnp;
use proc_macro2::Span;
use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
};
use syn::{
    Attribute, File, FnArg, ImplItem, ImplItemFn, Item, ItemFn, ItemImpl, Type, parse_file,
    parse_quote,
};
use toml_edit::{Array, DocumentMut, Item as TomlItem, Table, value};

const PRECOMPILED_CACHE_DIR: &str = "cache";
const PRECOMPILED_CACHE_RUST_DIR: &str = "rust";
const PRECOMPILED_DEPS_DIR: &str = "deps";
const PRECOMPILED_BUILD_DIR: &str = "build";
const PRECOMPILED_EXTERN_CRATES: [&str; 4] = ["peppylib", "capnp", "bytes", "tokio"];

pub fn add_peppylib_dependencies(to_path: impl AsRef<Path>) -> Result<()> {
    let to_path = to_path.as_ref();
    generate_lib_structure(to_path)?;
    let config_root =
        infer_node_root_from_peppygen_dir(to_path).unwrap_or_else(|| to_path.to_path_buf());
    let runtime_dir = copy_precompiled_runtime_bundle(&config_root)?;
    configure_cargo_for_precompiled_runtime(&config_root, &runtime_dir)?;
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

// ---------------------------------------------------------------------------
// Precompiled runtime deployment
// ---------------------------------------------------------------------------

fn copy_precompiled_runtime_bundle(config_root: &Path) -> Result<PathBuf> {
    let source_bundle = Path::new(env!("PEPPY_RUST_PRECOMPILED_BUNDLE_DIR"));
    if !source_bundle.exists() {
        return Err(Error::Io(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "missing precompiled Rust runtime bundle at {}",
                source_bundle.display()
            ),
        )));
    }

    let destination_bundle = runtime_cache_dir(config_root);
    stage_runtime_bundle(source_bundle, &destination_bundle)?;
    Ok(destination_bundle)
}

fn configure_cargo_for_precompiled_runtime(config_root: &Path, runtime_dir: &Path) -> Result<()> {
    let deps_dir = runtime_dir.join(PRECOMPILED_DEPS_DIR);
    let build_dir = runtime_dir.join(PRECOMPILED_BUILD_DIR);

    let extern_crate_rlibs = collect_runtime_extern_rlibs(&deps_dir)?;
    let mut native_dirs = collect_native_dirs(&build_dir)?;

    // Include system library search paths (e.g., OpenSSL) captured during precompilation
    let system_paths_file = runtime_dir.join("system-native-search-paths.txt");
    if system_paths_file.exists() {
        let contents = fs::read_to_string(&system_paths_file)?;
        for line in contents.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                native_dirs.push(PathBuf::from(trimmed));
            }
        }
    }

    native_dirs.sort();
    native_dirs.dedup();

    write_precompiled_rustflags(config_root, &extern_crate_rlibs, &deps_dir, &native_dirs)
}

fn runtime_cache_dir(config_root: &Path) -> PathBuf {
    config_root
        .join(PEPPY_OUTPUT_DIR)
        .join(PRECOMPILED_CACHE_DIR)
        .join(PRECOMPILED_CACHE_RUST_DIR)
        .join(env!("PEPPY_RUST_PRECOMPILED_TARGET"))
        .join(env!("PEPPY_RUST_PRECOMPILED_RUSTC"))
        .join(env!("PEPPY_RUST_PRECOMPILED_PROFILE_DIR"))
}

fn infer_node_root_from_peppygen_dir(peppygen_dir: &Path) -> Option<PathBuf> {
    let component_count = Path::new(PEPPYGEN_OUTPUT_PATH).components().count();
    let candidate_root = peppygen_dir.ancestors().nth(component_count)?;
    if candidate_root.join(PEPPYGEN_OUTPUT_PATH) == peppygen_dir {
        Some(candidate_root.to_path_buf())
    } else {
        None
    }
}

fn collect_runtime_extern_rlibs(deps_dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut externs = Vec::new();

    for crate_name in PRECOMPILED_EXTERN_CRATES {
        let rlib = find_crate_rlib(deps_dir, crate_name)?;
        externs.push((crate_name.to_owned(), rlib));
    }

    Ok(externs)
}

fn find_crate_rlib(deps_dir: &Path, crate_name: &str) -> Result<PathBuf> {
    let needle = format!("lib{crate_name}-");
    let mut candidates: Vec<PathBuf> = fs::read_dir(deps_dir)?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path.extension() == Some(OsStr::new("rlib"))
                && path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| name.starts_with(&needle))
        })
        .collect();

    candidates.sort();
    candidates.into_iter().next().ok_or_else(|| {
        Error::Io(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "missing precompiled crate `{crate_name}` rlib under {}",
                deps_dir.display()
            ),
        ))
    })
}

fn collect_native_dirs(build_dir: &Path) -> Result<Vec<PathBuf>> {
    if !build_dir.exists() {
        return Ok(Vec::new());
    }

    let mut dirs = Vec::new();
    collect_native_dirs_inner(build_dir, &mut dirs)?;
    Ok(dirs)
}

fn collect_native_dirs_inner(current: &Path, dirs: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            collect_native_dirs_inner(&path, dirs)?;
            continue;
        }

        if file_type.is_file()
            && path
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(is_native_lib_extension)
            && let Some(parent) = path.parent()
        {
            dirs.push(parent.to_path_buf());
        }
    }

    Ok(())
}

fn is_native_lib_extension(ext: &str) -> bool {
    matches!(ext, "a" | "so" | "dylib" | "dll" | "lib")
}

// ---------------------------------------------------------------------------
// .cargo/config.toml rustflags management
// ---------------------------------------------------------------------------

fn write_precompiled_rustflags(
    config_root: &Path,
    extern_crate_rlibs: &[(String, PathBuf)],
    deps_dir: &Path,
    native_dirs: &[PathBuf],
) -> Result<()> {
    let config_path = config_root.join(".cargo").join("config.toml");
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut doc: DocumentMut = if config_path.exists() {
        fs::read_to_string(&config_path).and_then(|contents| {
            contents.parse().map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("failed to parse {}: {err}", config_path.display()),
                )
            })
        })?
    } else {
        DocumentMut::new()
    };

    if !doc.as_table().contains_key("build") {
        doc["build"] = TomlItem::Table(Table::new());
    }

    let build_table = doc
        .get_mut("build")
        .and_then(TomlItem::as_table_mut)
        .ok_or_else(|| {
            Error::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected [build] table in {}", config_path.display()),
            ))
        })?;

    let mut flags = read_existing_rustflags(build_table);
    flags = strip_managed_rustflags(flags);

    for (crate_name, rlib_path) in extern_crate_rlibs {
        let rlib_path = normalize_path_for_rustflags(rlib_path);
        append_flag_pair_if_missing(&mut flags, "--extern", format!("{crate_name}={rlib_path}"));
    }
    let deps_dir = normalize_path_for_rustflags(deps_dir);
    append_flag_pair_if_missing(&mut flags, "-L", format!("dependency={deps_dir}"));

    for native_dir in native_dirs {
        let native_dir = normalize_path_for_rustflags(native_dir);
        append_flag_pair_if_missing(&mut flags, "-L", format!("native={native_dir}"));
    }

    let mut rustflags = Array::new();
    for flag in flags {
        rustflags.push(flag);
    }
    build_table["rustflags"] = value(rustflags);

    fs::write(config_path, doc.to_string())?;
    Ok(())
}

fn read_existing_rustflags(build_table: &Table) -> Vec<String> {
    let Some(existing) = build_table.get("rustflags") else {
        return Vec::new();
    };

    if let Some(array) = existing.as_array() {
        return array
            .iter()
            .filter_map(|value| value.as_str().map(ToOwned::to_owned))
            .collect();
    }

    if let Some(single) = existing.as_str() {
        return vec![single.to_owned()];
    }

    Vec::new()
}

fn append_flag_pair_if_missing(flags: &mut Vec<String>, flag: &str, value: String) {
    let mut idx = 0usize;
    while idx + 1 < flags.len() {
        if flags[idx] == flag && flags[idx + 1] == value {
            return;
        }
        idx += 1;
    }

    flags.push(flag.to_owned());
    flags.push(value);
}

fn normalize_path_for_rustflags(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn strip_managed_rustflags(flags: Vec<String>) -> Vec<String> {
    let mut retained = Vec::new();
    let mut idx = 0usize;

    while idx < flags.len() {
        if idx + 1 < flags.len() && is_managed_flag_pair(&flags[idx], &flags[idx + 1]) {
            idx += 2;
            continue;
        }

        retained.push(flags[idx].clone());
        idx += 1;
    }

    retained
}

fn is_managed_flag_pair(flag: &str, value: &str) -> bool {
    if flag == "--extern"
        && let Some((crate_name, _)) = value.split_once('=')
        && PRECOMPILED_EXTERN_CRATES.contains(&crate_name)
    {
        return true;
    }

    if flag == "-L" {
        if let Some(path) = value.strip_prefix("dependency=") {
            return is_managed_dependency_path(path);
        }
        if let Some(path) = value.strip_prefix("native=") {
            return is_managed_native_path(path);
        }
    }

    false
}

fn is_managed_dependency_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    normalized.contains("/.peppy/cache/rust/") && normalized.ends_with("/deps")
}

fn is_managed_native_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    normalized.contains("/.peppy/cache/rust/") && normalized.contains("/build/")
}

// ---------------------------------------------------------------------------
// Library structure generation
// ---------------------------------------------------------------------------

fn generate_lib_structure(to_path: impl AsRef<Path>) -> Result<()> {
    let to_path = to_path.as_ref();
    fs::create_dir_all(to_path)?;

    crate::generator::common::copy_embedded_templates("peppygen/rust", to_path)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_module_name_escapes_rust_keywords() {
        assert_eq!(sanitize_rust_module_name("mod"), "mod_");
        assert_eq!(sanitize_rust_module_name("type"), "type_");
    }
}
