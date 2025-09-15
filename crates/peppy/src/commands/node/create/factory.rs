use super::super::types::NodeName;
use crate::commands::node::create::{python, rust};
use crate::{Error, Result};
use askama::Template;
use config::{ConfigTemplateType, Language, NodeConfigCreator};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::info;

/// Trait for creating language-specific node configurations
pub trait NodeFactory {
    /// Context accessor for common node data
    fn ctx(&self) -> &NodeContext;

    /// Creates language-specific configuration files
    fn create_language_config(&self) -> Result<()>;

    /// Creates the gitignore file for this language
    fn create_gitignore(&self) -> Result<()>;

    /// Creates the peppy.json5 config (language-agnostic default)
    fn create_peppy_node_config(&self, full: bool) -> Result<PathBuf> {
        let ctx = self.ctx();
        let peppy_config_path = ctx.node_path.join("peppy.json5");

        let builder = if full {
            NodeConfigCreator::new(
                &ConfigTemplateType::FullNode,
                &ctx.node_name.as_str(),
                Some("/"),
                &ctx.language,
            )
        } else {
            NodeConfigCreator::new(
                &ConfigTemplateType::SimpleNode,
                &ctx.node_name.as_str(),
                Some("/"),
                &ctx.language,
            )
        };

        builder
            .write_to(&peppy_config_path)
            .map_err(Error::PeppyConfig)?;

        info!(
            "Created {} node in {}",
            &ctx.node_name,
            peppy_config_path.display()
        );
        Ok(peppy_config_path)
    }
}

/// Bundles common data needed to create a node
#[derive(Clone)]
pub struct NodeContext {
    pub language: Language,
    pub node_name: NodeName,
    pub node_path: PathBuf,
    pub description: String,
}

impl NodeContext {
    pub fn new(
        node_name: NodeName,
        node_path: impl AsRef<Path>,
        description: impl Into<String>,
        language: Language,
    ) -> Self {
        Self {
            language,
            node_name,
            node_path: node_path.as_ref().to_path_buf(),
            description: description.into(),
        }
    }
}

#[derive(Template)]
#[template(path = "gitignore/py.gitignore.j2")]
struct PythonGitignoreTemplate;

/// Factory for creating Python nodes
pub struct PythonNodeFactory {
    ctx: NodeContext,
}

impl PythonNodeFactory {
    pub fn new(ctx: NodeContext) -> Self {
        Self { ctx }
    }
}

impl NodeFactory for PythonNodeFactory {
    fn ctx(&self) -> &NodeContext {
        &self.ctx
    }

    fn create_language_config(&self) -> Result<()> {
        python::add_python_node_config(&self.ctx.node_name, &self.ctx.node_path)
            .map_err(|e| Error::PythonConfigCreation(e.to_string()))
    }

    fn create_gitignore(&self) -> Result<()> {
        let template = PythonGitignoreTemplate;
        let gitignore_content = template
            .render()
            .map_err(|e| Error::GitConfigCreation(e.to_string()))?;

        write_gitignore(&self.ctx.node_path, &gitignore_content)
    }
}

#[derive(Template)]
#[template(path = "gitignore/rust.gitignore.j2")]
struct RustGitignoreTemplate;

/// Factory for creating Rust nodes
pub struct RustNodeFactory {
    ctx: NodeContext,
}

impl RustNodeFactory {
    pub fn new(ctx: NodeContext) -> Self {
        Self { ctx }
    }
}

impl NodeFactory for RustNodeFactory {
    fn ctx(&self) -> &NodeContext {
        &self.ctx
    }

    fn create_language_config(&self) -> Result<()> {
        rust::add_rust_node_config(
            &self.ctx.node_name,
            &self.ctx.node_path,
            &self.ctx.description,
        )
        .map_err(|e| Error::RustConfigCreation(e.to_string()))
    }

    fn create_gitignore(&self) -> Result<()> {
        let template = RustGitignoreTemplate;
        let gitignore_content = template
            .render()
            .map_err(|e| Error::GitConfigCreation(e.to_string()))?;

        write_gitignore(&self.ctx.node_path, &gitignore_content)
    }
}

/// Creates a factory for the specified language
pub fn create_factory(ctx: NodeContext) -> Box<dyn NodeFactory> {
    match ctx.language {
        Language::Python => Box::new(PythonNodeFactory::new(ctx)),
        Language::Rust => Box::new(RustNodeFactory::new(ctx)),
    }
}

fn write_gitignore(node_path: &Path, content: &str) -> Result<()> {
    let gitignore_path = node_path.join(".gitignore");
    let mut file =
        fs::File::create(&gitignore_path).map_err(|e| Error::GitConfigCreation(e.to_string()))?;
    file.write_all(content.as_bytes())
        .map_err(|e| Error::GitConfigCreation(e.to_string()))?;
    Ok(())
}
