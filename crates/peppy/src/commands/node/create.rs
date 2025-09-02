mod factory;
mod pixi;
mod python;
mod rust;

use super::types::{Language, NodeName};
use crate::{Error, Result};
use config::create_peppy_node_config;
use factory::create_factory;
use std::fs;
use std::path::{Path, PathBuf};

pub struct NodeBuilder {
    current_dir: PathBuf,
    to_dir: Option<PathBuf>,
    node_name: NodeName,
    lang: Language,
    description: Option<String>,
    full: bool,
}

impl NodeBuilder {
    pub fn new(node_name: NodeName) -> Self {
        Self {
            current_dir: PathBuf::new(),
            to_dir: None,
            node_name,
            lang: Language::Rust,
            description: None,
            full: false,
        }
    }

    pub fn current_dir(mut self, dir: PathBuf) -> Self {
        self.current_dir = dir;
        self
    }

    pub fn to_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.to_dir = dir;
        self
    }

    pub fn lang(mut self, lang: Language) -> Self {
        self.lang = lang;
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
        create(
            &self.current_dir,
            self.to_dir.as_deref(),
            self.node_name,
            self.lang,
            self.description.as_deref(),
            self.full,
        )?;
        Ok(())
    }
}

/// Creates a new node and updates the peppy.star configuration file where the command is run
pub fn create(
    from_dir: &Path,
    to_dir: Option<&Path>,
    node_name: NodeName,
    language: Language,
    description: Option<&str>,
    full: bool,
) -> Result<()> {
    let node_path = match to_dir {
        Some(dir) => dir.join(node_name.as_str()),
        None => std::env::current_dir()?.join(node_name.as_str()),
    };

    if node_path.exists() {
        return Err(Error::FolderAlreadyExist(node_path.display().to_string()));
    }

    if !from_dir.join("peppy.yaml").exists() {
        return Err(Error::RootConfigurationNotFound);
    }

    fs::create_dir_all(&node_path)?;

    // Use factory pattern for language-specific operations
    let factory = create_factory(language);

    factory.create_gitignore(&node_path)?;
    factory.create_pixi_config(&node_path, &node_name, description)?;
    create_peppy_node_config(&node_path, node_name.as_str(), full)
        .map_err(|e| Error::PeppyConfigCreation(e.to_string()))?;
    factory.create_language_config(&node_name, &node_path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_root_node_config_missing() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        // Create from a directory without peppy.star
        let result = create(
            temp_dir.path(),
            None,
            NodeName::new("video_node").unwrap(),
            Language::Python,
            Some("Test video node"),
            false,
        );
        assert!(matches!(result, Err(Error::RootConfigurationNotFound)))
    }

    #[test]
    fn test_folder_already_exists_error() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let node_name = "existing_node";

        // Create peppy.yaml in the temp directory to avoid RootConfigurationNotFound error
        fs::write(temp_dir.path().join("peppy.yaml"), "# Root config").unwrap();

        // Create a directory with the same name as the node
        let existing_dir = temp_dir.path().join(node_name);
        fs::create_dir(&existing_dir).unwrap();

        // Try to create a node with the same name
        let result = create(
            temp_dir.path(),
            Some(temp_dir.path()),
            NodeName::new(node_name).unwrap(),
            Language::Python,
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
        use crate::commands::node::create::factory::{NodeFactory, PythonNodeFactory};
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let factory = PythonNodeFactory;

        let result = factory.create_gitignore(temp_dir.path());
        assert!(result.is_ok());

        let gitignore_path = temp_dir.path().join(".gitignore");
        assert!(gitignore_path.exists());

        let content = fs::read_to_string(gitignore_path).unwrap();
        assert!(content.contains("__pycache__"));
        assert!(content.contains(".pixi"));
    }

    #[test]
    fn test_create_gitignore_rust() {
        use crate::commands::node::create::factory::{NodeFactory, RustNodeFactory};
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let factory = RustNodeFactory;

        let result = factory.create_gitignore(temp_dir.path());
        assert!(result.is_ok());

        let gitignore_path = temp_dir.path().join(".gitignore");
        assert!(gitignore_path.exists());

        let content = fs::read_to_string(gitignore_path).unwrap();
        assert!(content.contains("/target/"));
        assert!(content.contains(".pixi"));
    }
}
