mod factory;
mod pixi;
mod python;
mod rust;

use super::error::NodeCommandError;
use super::types::{Language, NodeName};
use askama::Template;
use factory::create_factory;
use std::fs;
use std::io::Write;
use std::path::Path;

#[derive(Template)]
#[template(path = "peppy_new_node.star.j2")]
struct PeppyNodeTemplate<'a> {
    name: &'a str,
}

/// Creates a new node and updates the peppy.star configuration file where the command is run
pub fn create(
    from_dir: &Path,
    to_dir: Option<&Path>,
    node_name: NodeName,
    language: Language,
    description: Option<&str>,
) -> Result<(), NodeCommandError> {
    let node_path = match to_dir {
        Some(dir) => dir.join(node_name.as_str()),
        None => std::env::current_dir()?.join(node_name.as_str()),
    };

    if node_path.exists() {
        return Err(NodeCommandError::FolderAlreadyExist(
            node_path.display().to_string(),
        ));
    }

    if !from_dir.join("peppy.star").exists() {
        return Err(NodeCommandError::RootConfigurationNotFound);
    }

    fs::create_dir_all(&node_path)?;

    // Use factory pattern for language-specific operations
    let factory = create_factory(language);

    factory.create_gitignore(&node_path)?;
    factory.create_pixi_config(&node_path, &node_name, description)?;
    create_peppy_config(&node_path, &node_name)
        .map_err(|e| NodeCommandError::PeppyConfigCreation(e.to_string()))?;
    factory.create_language_config(&node_name, &node_path)?;

    println!("Created node '{}' at: {}", node_name, node_path.display());

    Ok(())
}

fn create_peppy_config(node_path: &Path, node_name: &NodeName) -> anyhow::Result<()> {
    let peppy_star_path = node_path.join("peppy.star");
    let mut file = fs::File::create(peppy_star_path)?;

    let template = PeppyNodeTemplate {
        name: node_name.as_str(),
    };
    let peppy_content = template.render()?;

    file.write_all(peppy_content.as_bytes())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Can be run from the command line with:
    // cargo run --manifest-path <path_to_root_Cargo.toml> -- node create my_project
    #[test]
    fn test_create_peppy_config() {
        use starlark::environment::{Globals, Module};
        use starlark::eval::Evaluator;
        use starlark::syntax::{AstModule, Dialect};
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let node_name = NodeName::new("test_node").unwrap();

        let result = create_peppy_config(temp_dir.path(), &node_name);
        assert!(result.is_ok());

        let peppy_path = temp_dir.path().join("peppy.star");
        assert!(peppy_path.exists());

        let content = fs::read_to_string(&peppy_path).unwrap();
        assert!(content.contains(node_name.as_str()));

        // Validate that the generated file is valid Starlark syntax
        let ast = AstModule::parse(
            &peppy_path.to_string_lossy(),
            content.clone(),
            &Dialect::Extended,
        );
        assert!(ast.is_ok(), "Failed to parse peppy.star as valid Starlark");

        // Also try to evaluate it to ensure it's not just syntactically valid
        // but also executable
        let ast_module = ast.unwrap();
        let globals = Globals::extended_internal();
        let module = Module::new();
        let mut evaluator = Evaluator::new(&module);
        let eval_result = evaluator.eval_module(ast_module, &globals);
        assert!(
            eval_result.is_ok(),
            "Failed to evaluate peppy.star: {:?}",
            eval_result.err()
        );
    }

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
        );
        assert!(matches!(
            result,
            Err(NodeCommandError::RootConfigurationNotFound)
        ))
    }

    #[test]
    fn test_folder_already_exists_error() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let node_name = "existing_node";

        // Create peppy.star in the temp directory to avoid RootConfigurationNotFound error
        fs::write(temp_dir.path().join("peppy.star"), "# Root config").unwrap();

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
        );

        assert!(matches!(
            result,
            Err(NodeCommandError::FolderAlreadyExist(_))
        ));

        if let Err(NodeCommandError::FolderAlreadyExist(path)) = result {
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
