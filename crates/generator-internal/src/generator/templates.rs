use crate::{
    error::{Error, Result},
    generator::types::{InterfaceArtifact, InterfaceKind},
};
use askama::Template;
use proc_macro2::Span;
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
};
use syn::{
    Attribute, File, FnArg, ImplItem, ImplItemFn, Item, ItemFn, ItemImpl, Type, parse_file,
    parse_quote,
};

#[derive(Template)]
#[template(path = "peppygen/rust/Cargo.toml.j2", escape = "none")]
struct PeppyConfigTemplate<'a> {
    peppylib_version: &'a str,
}

impl<'a> PeppyConfigTemplate<'a> {
    pub const TEMPLATE_PATH: &'static str = "peppygen/rust/Cargo.toml.j2";
}

pub fn generate_lib_structure(to_path: impl AsRef<Path>) -> Result<()> {
    let templates_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
    let template_root = templates_dir.join("peppygen/rust");
    let to_path = to_path.as_ref();

    fs::create_dir_all(to_path)?;

    copy_templates(&templates_dir, &template_root, to_path)
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

    write_lib_rs(&src_dir)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ModuleCategory {
    Topics,
    Services,
    Actions,
}

impl ModuleCategory {
    const ALL: [Self; 3] = [Self::Topics, Self::Services, Self::Actions];

    fn from_kind(kind: InterfaceKind) -> Self {
        match kind {
            InterfaceKind::ExposedTopic | InterfaceKind::SubscribedTopic => Self::Topics,
            InterfaceKind::ExposedService | InterfaceKind::SubscribedService => Self::Services,
            InterfaceKind::ExposedAction | InterfaceKind::SubscribedAction => Self::Actions,
        }
    }

    fn struct_name(self) -> &'static str {
        match self {
            Self::Topics => "Topics",
            Self::Services => "Services",
            Self::Actions => "Actions",
        }
    }
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
            .or_insert_with(BTreeMap::new)
            .entry(artifact.node_name.clone())
            .or_default()
            .push(artifact);
    }

    grouped
}

fn prepare_category_dir(src_dir: &Path, category: ModuleCategory) -> Result<PathBuf> {
    let category_dir = src_dir.join(category.struct_name().to_lowercase());
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
    src_dir.join(format!("{}.rs", category.struct_name().to_lowercase()))
}

fn write_lib_rs(src_dir: &Path) -> Result<()> {
    let lib_rs_path = src_dir.join("lib.rs");
    let mut content = String::new();
    for category in ModuleCategory::ALL {
        content.push_str(&format!(
            "pub mod {};\n",
            category.struct_name().to_lowercase()
        ));
    }
    fs::write(lib_rs_path, content)?;
    Ok(())
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
        let module_name = unique_module_name(&original_name, &mut counts);
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
    let mut content = String::new();
    for (module, original) in modules {
        if !original.is_empty() {
            content.push_str(&format!("// Node: {original}\n"));
        }
        content.push_str(&format!("pub mod {module};\n"));
    }
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
            category.struct_name().to_lowercase(),
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

fn unique_module_name(original: &str, counts: &mut HashMap<String, usize>) -> String {
    let base = sanitize_module_name(original);
    let counter = counts.entry(base.clone()).or_insert(0);
    let name = if *counter == 0 {
        base.clone()
    } else {
        format!("{base}_{}", counter)
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
        return "node".to_string();
    }

    if matches!(out.chars().next(), Some(ch) if ch.is_ascii_digit()) {
        out.insert(0, '_');
    }

    out
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
        let mut rendered_attrs = prettyplease::unparse(&attr_file);
        while rendered_attrs.ends_with('\n') {
            rendered_attrs.pop();
        }
        output.push_str(&rendered_attrs);
        output.push('\n');
    }

    let mut items_iter = module_file.items.into_iter().peekable();
    while let Some(item) = items_iter.next() {
        let item_file = File {
            shebang: None,
            attrs: Vec::new(),
            items: vec![item],
        };
        let mut rendered_item = prettyplease::unparse(&item_file);
        while rendered_item.ends_with('\n') {
            rendered_item.pop();
        }
        output.push_str(&rendered_item);
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
fn copy_templates(root: &Path, from: &Path, to: &Path) -> Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        let destination = to.join(entry.file_name());

        if file_type.is_dir() {
            fs::create_dir_all(&destination)?;
            copy_templates(root, &path, &destination)?;
        } else if file_type.is_file() {
            let ext = path.extension().and_then(|ext| ext.to_str());
            if ext == Some(".gitkeep") {
                continue;
            } else if ext == Some("j2") {
                let rendered = render_template(root, &path)?;
                let destination = destination.with_extension("");

                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }

                fs::write(&destination, rendered)?;
                continue;
            }

            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }

            fs::copy(&path, &destination)?;
        }
    }

    Ok(())
}

fn render_template(root: &Path, template_path: &Path) -> Result<String> {
    let relative = template_path
        .strip_prefix(root)
        .unwrap_or(template_path)
        .to_string_lossy()
        .into_owned();

    match relative.as_str() {
        PeppyConfigTemplate::TEMPLATE_PATH => {
            let tpl = PeppyConfigTemplate {
                peppylib_version: env!("CARGO_PKG_VERSION"),
            };
            Ok(tpl.render()?)
        }
        _ => Err(Error::UnknownTemplate(relative)),
    }
}
