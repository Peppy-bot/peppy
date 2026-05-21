use super::identifiers::is_rust_keyword;
use crate::generator::common::{
    CrateDeployMode, EmbeddedBuildHelpers, EmbeddedConfigInternal, EmbeddedCoreNodeApi,
    EmbeddedPeppylib, EmbeddedPmiInternal, WorkspacePackageMetadata, cache_sibling_path,
    copy_dir_recursive,
};
use crate::{
    error::{Error, Result},
    generator::{
        naming::{sanitize_component, unique_module_name},
        scaffold_tree::{ModuleTree, build_module_tree},
        types::{CapnpSchema, InterfaceArtifact, ModuleCategory},
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
        let entries = grouped.remove(&category).unwrap_or_default();
        let tree = build_module_tree(entries);
        write_module_tree(&category_dir, &category_file, &tree, category)?;
    }

    Ok(())
}

/// Generates and writes the parameters struct to the library.
///
/// This function takes the node's parameters configuration and generates a Rust struct
/// that can be used to deserialize parameter values at runtime.
pub fn add_parameters_to_lib(
    lib_path: impl AsRef<Path>,
    parameters: &config::ParameterSchema,
) -> Result<()> {
    let lib_path = lib_path.as_ref();
    let src_dir = lib_path.join("src");
    fs::create_dir_all(&src_dir)?;

    let parameters_file = src_dir.join("parameters.rs");
    let code = super::generate_parameters_struct(parameters)?;
    fs::write(&parameters_file, code)?;

    Ok(())
}

/// Writes `src/consumer_dependencies.rs` carrying the precomputed
/// `(producer_name, producer_tag) → pinned_link_ids` map and an
/// `ensure_registered(&MessengerHandle)` helper that installs it on the
/// first call. Each generated consumed-interface function invokes
/// `ensure_registered` before its `subscribe` / `poll` / `send_goal` so the
/// runtime sibling-precedence lookup is always seeded.
///
/// The helper uses a `OnceLock` per process: registration happens at most
/// once. If the user constructs multiple `MessengerHandle`s in one process
/// (an unusual pattern), only the first observes the registration. The
/// generated map is the same regardless of which handle it lands on, so
/// per-handle re-registration would be redundant.
pub fn add_consumer_dependencies_to_lib(
    lib_path: impl AsRef<Path>,
    pinned_siblings_map: &std::collections::HashMap<(String, String), Vec<String>>,
) -> Result<()> {
    let lib_path = lib_path.as_ref();
    let src_dir = lib_path.join("src");
    fs::create_dir_all(&src_dir)?;

    let mut entries: Vec<(&(String, String), &Vec<String>)> = pinned_siblings_map.iter().collect();
    // Sort for deterministic output; fingerprint stability depends on it.
    entries.sort_by(|a, b| a.0.cmp(b.0));

    let inserts: Vec<String> = entries
        .iter()
        .map(|((name, tag), link_ids)| {
            let lits: Vec<String> = link_ids
                .iter()
                .map(|s| format!("{:?}.to_string()", s))
                .collect();
            format!(
                "        map.insert(\n            ({:?}.to_string(), {:?}.to_string()),\n            vec![{}],\n        );",
                name,
                tag,
                lits.join(", ")
            )
        })
        .collect();

    let (register_param, body) = if inserts.is_empty() {
        (
            "_messenger",
            "    // No pinned-sibling dependencies declared in this node's manifest.\n".to_string(),
        )
    } else {
        (
            "messenger",
            format!(
                "    let mut map: std::collections::HashMap<(String, String), Vec<String>> =\n        std::collections::HashMap::new();\n{}\n    messenger.register_consumer_dependencies(map);\n",
                inserts.join("\n")
            ),
        )
    };

    let code = format!(
        "//! Auto-generated. Installs the consumer's `depends_on` pinned-sibling\n\
         //! map on the messenger so `from_any: true` dependencies skip producer\n\
         //! link_ids already claimed by a sibling pinned entry on the same\n\
         //! `(producer_name, producer_tag)`.\n\
         \n\
         use std::sync::OnceLock;\n\
         use peppylib::MessengerHandle;\n\
         \n\
         /// Idempotent registration entry point. Called by every generated\n\
         /// consumed-interface function before it talks to the messenger.\n\
         /// The `OnceLock` ensures the registration happens exactly once per\n\
         /// process even when many consumed interfaces are exercised.\n\
         pub fn ensure_registered(messenger: &MessengerHandle) {{\n\
             static REGISTERED: OnceLock<()> = OnceLock::new();\n\
             REGISTERED.get_or_init(|| {{\n\
                 register(messenger);\n\
             }});\n\
         }}\n\
         \n\
         fn register({register_param}: &MessengerHandle) {{\n\
         {body}}}\n",
    );

    fs::write(src_dir.join("consumer_dependencies.rs"), code)?;
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

/// Deploys the five vendored Rust crates (peppylib, pmi-internal, config-internal, core-node-api, build-helpers-internal)
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
        copy_embedded_crate::<EmbeddedCoreNodeApi>("core-node-api", &staging_dir, &metadata)?;
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

    // Link or copy all five crates (peppylib, pmi-internal, config-internal,
    // core-node-api, build-helpers-internal) into node_libs_dir.
    // All five are needed because the crates reference each other via relative
    // sibling paths (e.g., peppylib has `config = { path = "../config-internal" }`
    // and `build-helpers = { path = "../build-helpers-internal" }` in build-dependencies),
    // and Cargo resolves these paths relative to the symlink location, not the target.
    for crate_name in &[
        "peppylib",
        "pmi-internal",
        "config-internal",
        "core-node-api",
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

impl ModuleCategory {
    fn struct_name(self) -> &'static str {
        match self {
            Self::EmittedTopics => "EmittedTopics",
            Self::ConsumedTopics => "ConsumedTopics",
            Self::ExposedServices => "ExposedServices",
            Self::ConsumedServices => "ConsumedServices",
            Self::ExposedActions => "ExposedActions",
            Self::ConsumedActions => "ConsumedActions",
        }
    }

    fn doc_label(self) -> &'static str {
        match self {
            Self::EmittedTopics => "emitted topics",
            Self::ConsumedTopics => "consumed topics",
            Self::ExposedServices => "exposed services",
            Self::ConsumedServices => "consumed services",
            Self::ExposedActions => "exposed actions",
            Self::ConsumedActions => "consumed actions",
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
) -> BTreeMap<ModuleCategory, Vec<InterfaceArtifact>> {
    let mut grouped: BTreeMap<ModuleCategory, Vec<InterfaceArtifact>> = BTreeMap::new();

    for artifact in artifacts {
        let category = ModuleCategory::from_kind(artifact.kind);
        grouped.entry(category).or_default().push(artifact);
    }

    grouped
}

fn prepare_category_dir(src_dir: &Path, category: ModuleCategory) -> Result<PathBuf> {
    let category_dir = src_dir.join(category.dir_name());
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
    src_dir.join(format!("{}.rs", category.dir_name()))
}

/// Walks the module tree, writing a `.rs` file per leaf and a `mod.rs` (or
/// the category file at the root) listing the modules at each level.
///
/// `category_dir` is the on-disk directory under `src/` for this category
/// (e.g. `src/emitted_topics`). `category_file` is its companion sibling
/// `src/emitted_topics.rs` that declares `pub mod <subdir>;` for each top
/// child.
fn write_module_tree(
    category_dir: &Path,
    category_file: &Path,
    tree: &ModuleTree,
    category: ModuleCategory,
) -> Result<()> {
    let listing = write_tree_node(category_dir, tree, category)?;
    fs::write(category_file, listing)?;
    Ok(())
}

/// Recursive worker. Returns the rendered `pub mod ...;` listing for the
/// current directory; the caller writes it as `mod.rs` (or as the category
/// `.rs` companion file at the root). Dedupes sibling segments via
/// [`unique_module_name`] independently at each level so a leaf `video_stream`
/// can coexist with a child directory of the same name (e.g. when a native
/// topic shares its name with a conformed-interface tag — extremely unlikely,
/// but cheap to handle).
fn write_tree_node(dir: &Path, tree: &ModuleTree, category: ModuleCategory) -> Result<String> {
    fs::create_dir_all(dir)?;
    let mut listing = String::new();
    let mut counts: HashMap<String, usize> = HashMap::new();

    // Order: leaves first, then sub-directories. `BTreeMap` already gives us
    // deterministic alphabetical ordering inside each bucket.
    for (raw_leaf, artifacts) in &tree.leaves {
        let module_name = unique_module_name(raw_leaf, &mut counts, sanitize_rust_module_name);
        write_leaf_module(dir, &module_name, raw_leaf, artifacts.clone(), category)?;
        if !raw_leaf.is_empty() && raw_leaf != &module_name {
            listing.push_str(&format!("// {raw_leaf}\n"));
        }
        listing.push_str(&format!("pub mod {module_name};\n"));
    }

    for (raw_segment, child) in &tree.children {
        let module_name = unique_module_name(raw_segment, &mut counts, sanitize_rust_module_name);
        let child_dir = dir.join(&module_name);
        let child_listing = write_tree_node(&child_dir, child, category)?;
        fs::write(child_dir.join("mod.rs"), child_listing)?;
        listing.push_str(&format!("pub mod {module_name};\n"));
    }

    Ok(listing)
}

fn write_leaf_module(
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
    for artifact in artifacts {
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
