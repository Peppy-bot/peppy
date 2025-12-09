mod factory;
mod python;
mod rust;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::PackageManager;
use super::types::NodeName;
use crate::{AppContext, Error, Result};
use factory::{NodeContext, create_factory};

pub struct NodeBuilder {
    to_dir: PathBuf,
    node_name: NodeName,
    package_manager: PackageManager,
    description: Option<String>,
    full: bool,
}

impl NodeBuilder {
    pub fn new(ctx: &Arc<AppContext>, node_name: NodeName) -> Self {
        Self {
            to_dir: ctx.root_dir.clone(),
            node_name,
            package_manager: PackageManager::Rust,
            description: None,
            full: false,
        }
    }

    /// If to_dir is provided, this is the path used for NodeBuilder, otherwise defaults to the one
    /// provided by AppContext
    pub fn to_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.to_dir = PathBuf::from(dir.as_ref());
        self
    }

    pub fn lang(mut self, lang: PackageManager) -> Self {
        self.package_manager = lang;
        self
    }

    pub fn description(mut self, description: Option<String>) -> Self {
        self.description = description;
        self
    }

    pub fn full(mut self, full: bool) -> Self {
        self.full = full;
        self
    }

    pub fn build(self) -> Result<()> {
        init_project(
            self.to_dir,
            self.node_name,
            self.package_manager,
            self.description.as_deref(),
            self.full,
        )?;
        Ok(())
    }
}

/// Creates a new node and updates the peppy.json5 configuration file where the command is run
pub fn init_project(
    to_dir: impl AsRef<Path>,
    node_name: NodeName,
    language: PackageManager,
    description: Option<&str>,
    full: bool,
) -> Result<()> {
    let node_path = to_dir.as_ref().join(node_name.as_str());

    if node_path.exists() {
        return Err(Error::FolderAlreadyExist(node_path.display().to_string()));
    }

    fs::create_dir_all(&node_path)?;

    // Use factory pattern for language-specific operations
    let ctx = NodeContext::new(
        node_name.clone(),
        &node_path,
        description.unwrap_or(&format!("{} {} node", node_name.as_str(), language)),
        language,
    );

    let factory = create_factory(ctx);

    factory.create_gitignore()?;
    factory
        .create_peppy_node_config(full)
        .map_err(|e| Error::PeppyConfigCreation(e.to_string()))?;
    factory.create_language_config()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Can be run from the command line with:
    // cargo run --manifest-path <path_to_root_Cargo.toml> -- node create my_node
    #[test]
    fn test_create_peppy_config() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let node_name = "test_node";

        let ctx = NodeContext::new(
            NodeName::new(node_name).unwrap(),
            temp_dir.path(),
            "Test node",
            PackageManager::Rust,
        );
        let factory = create_factory(ctx);

        let result = factory.create_peppy_node_config(false);
        assert!(result.is_ok());

        let peppy_path = temp_dir.path().join("peppy.json5");
        assert!(peppy_path.exists());
    }

    #[test]
    fn test_folder_already_exists_error() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let node_name = "existing_node";

        // Create a directory with the same name as the node
        let existing_dir = temp_dir.path().join(node_name);
        fs::create_dir(&existing_dir).unwrap();

        // Try to create a node with the same name
        let result = init_project(
            temp_dir.path(),
            NodeName::new(node_name).unwrap(),
            PackageManager::Python,
            Some("Test node"),
            false,
        );

        assert!(matches!(result, Err(Error::FolderAlreadyExist(_))));

        if let Err(Error::FolderAlreadyExist(path)) = result {
            assert!(path.contains(node_name));
        }
    }

    #[test]
    fn test_create_gitignore_python() {
        use super::factory::{NodeContext, NodeFactory, PythonNodeFactory};
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let ctx = NodeContext::new(
            NodeName::new("py_node").unwrap(),
            temp_dir.path(),
            "desc",
            PackageManager::Python,
        );
        let factory = PythonNodeFactory::new(ctx);

        let result = factory.create_gitignore();
        assert!(result.is_ok());

        let gitignore_path = temp_dir.path().join(".gitignore");
        assert!(gitignore_path.exists());

        let content = fs::read_to_string(gitignore_path).unwrap();
        assert!(content.contains("__pycache__"));
    }

    #[test]
    fn test_create_gitignore_rust() {
        use super::factory::{NodeContext, NodeFactory, RustNodeFactory};
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let ctx = NodeContext::new(
            NodeName::new("rs_node").unwrap(),
            temp_dir.path(),
            "desc",
            PackageManager::Rust,
        );
        let factory = RustNodeFactory::new(ctx);

        let result = factory.create_gitignore();
        assert!(result.is_ok());

        let gitignore_path = temp_dir.path().join(".gitignore");
        assert!(gitignore_path.exists());

        let content = fs::read_to_string(gitignore_path).unwrap();
        assert!(content.contains("/target/"));
    }
}
