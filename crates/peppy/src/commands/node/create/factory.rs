use super::super::types::{Language, NodeName};
use crate::commands::node::create::{python, rust};
use crate::{Error, Result};
use askama::Template;
use std::fs;
use std::io::Write;
use std::path::Path;

/// Trait for creating language-specific node configurations
pub trait NodeFactory {
    /// Returns the language this factory handles
    fn language(&self) -> Language;

    /// Creates language-specific configuration files
    fn create_language_config(
        &self,
        node_name: &NodeName,
        node_path: &Path,
        description: &str,
    ) -> Result<()>;

    /// Creates the gitignore file for this language
    fn create_gitignore(&self, node_path: &Path) -> Result<()>;
}

/// Factory for creating Python nodes
pub struct PythonNodeFactory;

#[derive(Template)]
#[template(path = "gitignore/py.gitignore.j2")]
struct PythonGitignoreTemplate;

impl NodeFactory for PythonNodeFactory {
    fn language(&self) -> Language {
        Language::Python
    }

    fn create_language_config(
        &self,
        node_name: &NodeName,
        node_path: &Path,
        _description: &str,
    ) -> Result<()> {
        python::add_python_node_config(node_name, node_path)
            .map_err(|e| Error::PythonConfigCreation(e.to_string()))
    }

    fn create_gitignore(&self, node_path: &Path) -> Result<()> {
        let template = PythonGitignoreTemplate;
        let gitignore_content = template
            .render()
            .map_err(|e| Error::GitConfigCreation(e.to_string()))?;

        let gitignore_path = node_path.join(".gitignore");
        let mut file = fs::File::create(&gitignore_path)
            .map_err(|e| Error::GitConfigCreation(e.to_string()))?;
        file.write_all(gitignore_content.as_bytes())
            .map_err(|e| Error::GitConfigCreation(e.to_string()))?;
        Ok(())
    }
}

/// Factory for creating Rust nodes
pub struct RustNodeFactory;

#[derive(Template)]
#[template(path = "gitignore/rust.gitignore.j2")]
struct RustGitignoreTemplate;

impl NodeFactory for RustNodeFactory {
    fn language(&self) -> Language {
        Language::Rust
    }

    fn create_language_config(
        &self,
        node_name: &NodeName,
        node_path: &Path,
        description: &str,
    ) -> Result<()> {
        rust::add_rust_node_config(node_name, node_path, description)
            .map_err(|e| Error::RustConfigCreation(e.to_string()))
    }

    fn create_gitignore(&self, node_path: &Path) -> Result<()> {
        let template = RustGitignoreTemplate;
        let gitignore_content = template
            .render()
            .map_err(|e| Error::GitConfigCreation(e.to_string()))?;

        let gitignore_path = node_path.join(".gitignore");
        let mut file = fs::File::create(&gitignore_path)
            .map_err(|e| Error::GitConfigCreation(e.to_string()))?;
        file.write_all(gitignore_content.as_bytes())
            .map_err(|e| Error::GitConfigCreation(e.to_string()))?;
        Ok(())
    }
}

/// Creates a factory for the specified language
pub fn create_factory(language: Language) -> Box<dyn NodeFactory> {
    match language {
        Language::Python => Box::new(PythonNodeFactory),
        Language::Rust => Box::new(RustNodeFactory),
    }
}
