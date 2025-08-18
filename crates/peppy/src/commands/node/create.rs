mod pixi;
mod python;
mod rust;

use super::error::NodeCreationError;
use super::types::{Language, NodeName};
use askama::Template;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::str::FromStr;

#[derive(Template)]
#[template(path = "peppy_new_node.star.j2")]
struct PeppyNodeTemplate<'a> {
    name: &'a str,
}

#[derive(Template)]
#[template(path = "gitignore/py.gitignore.j2")]
struct PythonGitignoreTemplate;

#[derive(Template)]
#[template(path = "gitignore/rust.gitignore.j2")]
struct RustGitignoreTemplate;

/// Creates a new node and updates the peppy.star configuration file where the command is run
pub fn create(
    from_dir: &Path,
    to_dir: Option<&Path>,
    node_name: &str,
    lang: &str,
    description: Option<&str>,
) -> Result<(), NodeCreationError> {
    let language = Language::from_str(lang)?;
    let node_name = NodeName::new(node_name)?;

    let node_path = match to_dir {
        Some(dir) => dir.join(node_name.as_str()),
        None => std::env::current_dir()
            .map_err(NodeCreationError::CurrentDir)?
            .join(node_name.as_str()),
    };

    if node_path.exists() {
        return Err(NodeCreationError::FolderAlreadyExist(
            node_path.display().to_string(),
        ));
    }

    if !from_dir.join("peppy.star").exists() {
        return Err(NodeCreationError::RootConfigurationNotFound);
    }

    fs::create_dir_all(&node_path)?;

    create_gitignore(&node_path, language)?;
    pixi::create_pixi_toml(&node_path, &node_name, language, description)?;
    create_peppy_config(&node_path, &node_name)?;

    match language {
        Language::Python => python::add_python_node_config(&node_name, &node_path)?,
        Language::Rust => rust::add_rust_node_config(&node_name, &node_path)?,
    }

    println!("Created node '{}' at: {}", node_name, node_path.display());

    Ok(())
}

fn create_gitignore(node_path: &Path, lang: Language) -> Result<(), NodeCreationError> {
    let gitignore_content = match lang {
        Language::Python => {
            let template = PythonGitignoreTemplate;
            template.render().map_err(|e| {
                NodeCreationError::DirectoryCreation(std::io::Error::other(format!(
                    "Failed to render Python gitignore template: {}",
                    e
                )))
            })?
        }
        Language::Rust => {
            let template = RustGitignoreTemplate;
            template.render().map_err(|e| {
                NodeCreationError::DirectoryCreation(std::io::Error::other(format!(
                    "Failed to render Rust gitignore template: {}",
                    e
                )))
            })?
        }
    };

    let gitignore_path = node_path.join(".gitignore");
    let mut file = fs::File::create(&gitignore_path)?;
    file.write_all(gitignore_content.as_bytes())?;
    Ok(())
}

fn create_peppy_config(node_path: &Path, node_name: &NodeName) -> Result<(), NodeCreationError> {
    let peppy_star_path = node_path.join("peppy.star");
    let mut file = fs::File::create(peppy_star_path)?;

    let template = PeppyNodeTemplate {
        name: node_name.as_str(),
    };
    let peppy_content = template.render().map_err(|e| {
        NodeCreationError::DirectoryCreation(std::io::Error::other(format!(
            "Failed to render peppy template: {}",
            e
        )))
    })?;

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
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let node_name = NodeName::new("test_node").unwrap();

        let result = create_peppy_config(temp_dir.path(), &node_name);
        assert!(result.is_ok());

        let peppy_path = temp_dir.path().join("peppy.star");
        assert!(peppy_path.exists());

        let content = fs::read_to_string(peppy_path).unwrap();
        assert!(content.contains(node_name.as_str()));
    }

    #[test]
    fn test_check_root_node_config_missing() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        // Create from a directory without peppy.star
        let result = create(
            temp_dir.path(),
            None,
            "video_node",
            "python",
            Some("Test video node"),
        );
        assert!(matches!(
            result,
            Err(NodeCreationError::RootConfigurationNotFound)
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
            node_name,
            "python",
            Some("Test node"),
        );

        assert!(matches!(
            result,
            Err(NodeCreationError::FolderAlreadyExist(_))
        ));

        if let Err(NodeCreationError::FolderAlreadyExist(path)) = result {
            assert!(path.contains(node_name));
        }
    }

    #[test]
    fn test_create_gitignore_python() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();

        let result = create_gitignore(temp_dir.path(), Language::Python);
        assert!(result.is_ok());

        let gitignore_path = temp_dir.path().join(".gitignore");
        assert!(gitignore_path.exists());

        let content = fs::read_to_string(gitignore_path).unwrap();
        assert!(content.contains("__pycache__"));
        assert!(content.contains(".pixi"));
    }

    #[test]
    fn test_create_gitignore_rust() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();

        let result = create_gitignore(temp_dir.path(), Language::Rust);
        assert!(result.is_ok());

        let gitignore_path = temp_dir.path().join(".gitignore");
        assert!(gitignore_path.exists());

        let content = fs::read_to_string(gitignore_path).unwrap();
        assert!(content.contains("/target/"));
        assert!(content.contains(".pixi"));
    }
}
