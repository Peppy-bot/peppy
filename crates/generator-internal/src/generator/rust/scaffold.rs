use super::identifiers::is_rust_keyword;
use crate::generator::common::{
    CrateDeployMode, EmbeddedBuildHelpers, EmbeddedConfig, EmbeddedCoreNodeApi,
    EmbeddedPeppyMessagingInterface, EmbeddedPeppylib, WorkspacePackageMetadata,
    cache_sibling_path, copy_dir_recursive,
};
use crate::{
    error::{Error, Result},
    generator::{
        naming::sanitize_component,
        scaffold_tree::{
            ModuleEntry, TreeWriter, build_module_tree, group_artifacts_by_category,
            write_module_tree,
        },
        types::{CapnpSchema, InterfaceArtifact, ModuleCategory},
    },
};
use encoding::compile_capnp;
use proc_macro2::Span;
use rust_embed::Embed;
use sha2::{Digest, Sha256};
use std::io;
use std::io::ErrorKind;
use std::{
    collections::HashMap,
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
        let mut writer = RustTreeWriter {
            category,
            category_file: &category_file,
        };
        write_module_tree(&category_dir, &tree, &mut writer)?;
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

/// Normalizes a vendored crate's `Cargo.toml` for the flat `.peppy/libs` layout:
/// replaces workspace-inherited fields (`version.workspace = true`, etc.) with
/// concrete values, renames the `peppylib-rs` source package back to `peppylib`
/// (the name every generated node depends on), and flattens inter-crate `path`
/// dependencies to flat siblings.
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

        // The crate is named `peppylib-rs` in the peppyos-shared workspace, but it
        // is vendored into `.peppy/libs/peppylib` (lib name already `peppylib`).
        // Rename the package back to `peppylib` so every generated node's
        // `peppylib = { path = ".peppy/libs/peppylib" }` resolves unchanged.
        if package.get("name").and_then(|n| n.as_str()) == Some("peppylib-rs") {
            package.insert("name", value("peppylib"));
        }
    }

    normalize_vendored_path_deps(&mut doc);

    fs::write(cargo_toml_path, doc.to_string())?;
    Ok(())
}

/// Crate directories the vendored `.peppy/libs` cache lays out as flat siblings.
/// A `path` dependency whose final component is one of these is rewritten to
/// `../<crate>` so it resolves in the flat cache regardless of whether the source
/// manifest used a flat path (`../peppy-config-model`) or a reverse path into another
/// submodule (`../../../peppyos/crates/config`, as `peppylib-rs` does).
const VENDORED_SIBLING_CRATES: &[&str] = &[
    "peppylib",
    "peppy-messaging-interface",
    "peppy-config-model",
    "core-node-api",
    "build-helpers",
];

/// Returns the flattened `../<crate>` path for `current` if its final component
/// names a vendored sibling crate, otherwise `None` (leave it untouched).
fn flatten_vendored_path(current: &str) -> Option<String> {
    let final_component = current.rsplit('/').next().unwrap_or(current);
    VENDORED_SIBLING_CRATES
        .contains(&final_component)
        .then(|| format!("../{final_component}"))
}

/// Rewrites every intra-vendor `path` dependency in `doc` to a flat sibling so the
/// deployed flat-cache layout resolves even when the source manifest pointed across
/// submodule boundaries. Registry deps and non-vendored path deps are left alone;
/// `features` and other keys on the dependency are preserved. Idempotent on the
/// already-flat crates that have not been moved out of the peppyos workspace.
fn normalize_vendored_path_deps(doc: &mut DocumentMut) {
    for table_name in ["dependencies", "dev-dependencies", "build-dependencies"] {
        let Some(deps) = doc.get_mut(table_name).and_then(|t| t.as_table_mut()) else {
            continue;
        };
        for (_dep_name, item) in deps.iter_mut() {
            // Inline form: `dep = { path = "...", features = [...] }`.
            if let Some(inline) = item.as_inline_table_mut() {
                if let Some(path_val) = inline.get_mut("path")
                    && let Some(flat) = path_val.as_str().and_then(flatten_vendored_path)
                {
                    *path_val = flat.as_str().into();
                }
            // Full-table form: `[dependencies.dep]\npath = "..."`.
            } else if let Some(table) = item.as_table_mut()
                && let Some(path_item) = table.get_mut("path")
                && let Some(flat) = path_item.as_str().and_then(flatten_vendored_path)
            {
                *path_item = value(flat);
            }
        }
    }
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

/// Mixes every file of one embedded crate into `hasher` in a deterministic
/// (sorted-by-path) order. A per-crate `label` and NUL domain separators keep
/// identically named files in different crates from colliding.
fn hash_embedded_crate<E: Embed>(hasher: &mut Sha256, label: &str) {
    hasher.update(label.as_bytes());
    hasher.update([0u8]);
    let mut names: Vec<String> = E::iter().map(|name| name.as_ref().to_string()).collect();
    names.sort();
    for name in names {
        // `get` cannot legitimately miss a name just returned by `iter`; skip
        // defensively rather than poison the whole key on a transient race.
        if let Some(file) = E::get(&name) {
            hasher.update(name.as_bytes());
            hasher.update([0u8]);
            hasher.update(file.data.as_ref());
            hasher.update([0u8]);
        }
    }
}

/// Cache key for the vendored Rust crates, derived from the **actual embedded
/// bytes that are about to be deployed** rather than a compile-time source
/// hash.
///
/// In debug builds `rust-embed` (no `debug-embed` feature) reads each crate's
/// source live from disk at call time, so this hash reflects the current
/// working tree — never a stale snapshot baked into the generator binary at its
/// last compile. That makes the shared cache self-invalidating: any source
/// change yields a new key and therefore a fresh deploy, even when cargo did not
/// rebuild the generator (e.g. a submodule checkout whose mtimes did not advance,
/// or a newly added file the build script's `rerun-if-changed` list never
/// watched). Keying on the deployed bytes also means the hash can never drift
/// out of agreement with the embed's own include/exclude rules.
///
/// In release builds the embed is compile-time, so `get` returns the embedded
/// bytes and this stays in lockstep with them (the build script's
/// `rerun-if-changed` registration keeps those embedded bytes fresh).
fn vendored_crates_cache_key() -> String {
    let mut hasher = Sha256::new();
    hash_embedded_crate::<EmbeddedPeppylib>(&mut hasher, "peppylib");
    hash_embedded_crate::<EmbeddedPeppyMessagingInterface>(
        &mut hasher,
        "peppy-messaging-interface",
    );
    hash_embedded_crate::<EmbeddedConfig>(&mut hasher, "peppy-config-model");
    hash_embedded_crate::<EmbeddedCoreNodeApi>(&mut hasher, "core-node-api");
    hash_embedded_crate::<EmbeddedBuildHelpers>(&mut hasher, "build-helpers");
    let hash = hasher.finalize();
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}", &hex[..16], env!("CARGO_PKG_VERSION"))
}

/// Deploys the vendored Rust crates (peppylib, peppy-messaging-interface, config, core-node-api,
/// build-helpers) to a shared cache directory, then links or
/// copies them into `node_libs_dir`.
///
/// In `Symlink` mode (the default), creates symlinks from `node_libs_dir/{crate}`
/// to the shared cache. This avoids duplicating source files across nodes.
///
/// In `Copy` mode, copies the crate sources directly into `node_libs_dir`.
/// This is needed for container builds where symlinks to host paths would break.
///
/// The cache is keyed by a content hash of the embedded crate bytes
/// ([`vendored_crates_cache_key`]) + version, and uses file locking with a
/// staging directory for concurrent-safe deployment.
fn deploy_rust_crates_to_shared_cache(
    node_libs_dir: &Path,
    peppy_dirs: &config::consts::PeppyDirs,
    deploy_mode: CrateDeployMode,
) -> Result<()> {
    let cache_key = vendored_crates_cache_key();
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
        copy_embedded_crate::<EmbeddedPeppyMessagingInterface>(
            "peppy-messaging-interface",
            &staging_dir,
            &metadata,
        )?;
        copy_embedded_crate::<EmbeddedConfig>("peppy-config-model", &staging_dir, &metadata)?;
        copy_embedded_crate::<EmbeddedCoreNodeApi>("core-node-api", &staging_dir, &metadata)?;
        copy_embedded_crate::<EmbeddedBuildHelpers>("build-helpers", &staging_dir, &metadata)?;

        if cache_dir.exists() {
            fs::remove_dir_all(&cache_dir)?;
        }
        fs::rename(&staging_dir, &cache_dir)?;
        fs::write(cache_dir.join(".complete"), "")?;
    }
    drop(lock_file);

    // Link or copy all vendored crates (peppylib, peppy-messaging-interface, config,
    // core-node-api, build-helpers) into node_libs_dir.
    // All are needed because the crates reference each other via relative sibling
    // paths (e.g., peppylib has `config = { package = "peppy-config-model", path = "../peppy-config-model" }` and
    // build-dependencies), and Cargo resolves these paths relative to the symlink
    // location, not the target.
    for crate_name in &[
        "peppylib",
        "peppy-messaging-interface",
        "peppy-config-model",
        "core-node-api",
        "build-helpers",
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
/// [`TreeWriter`] that renders Rust modules: one `.rs` file per leaf and a
/// `pub mod ...;` listing per directory, written to `mod.rs` (or, at the root,
/// to the category's sibling `.rs` companion file). A renamed leaf keeps a
/// `// <raw>` provenance comment. The traversal and sibling de-duplication live
/// in the shared [`write_module_tree`] walker.
struct RustTreeWriter<'a> {
    category: ModuleCategory,
    category_file: &'a Path,
}

impl TreeWriter for RustTreeWriter<'_> {
    fn sanitize_module_name(raw: &str) -> String {
        sanitize_rust_module_name(raw)
    }

    fn write_leaf(
        &mut self,
        dir: &Path,
        module_name: &str,
        raw_name: &str,
        artifacts: &[InterfaceArtifact],
    ) -> Result<()> {
        write_leaf_module(
            dir,
            module_name,
            raw_name,
            artifacts.to_vec(),
            self.category,
        )
    }

    fn write_index(&mut self, dir: &Path, entries: &[ModuleEntry], is_root: bool) -> Result<()> {
        let mut listing = String::new();
        for entry in entries {
            if entry.is_leaf && !entry.raw_name.is_empty() && entry.raw_name != entry.module_name {
                listing.push_str(&format!("// {}\n", entry.raw_name));
            }
            listing.push_str(&format!("pub mod {};\n", entry.module_name));
        }
        let target = if is_root {
            self.category_file.to_path_buf()
        } else {
            dir.join("mod.rs")
        };
        fs::write(target, listing)?;
        Ok(())
    }
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
