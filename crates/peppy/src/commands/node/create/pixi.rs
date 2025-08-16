use std::fs;
use std::io::Write;
use std::path::Path;

use askama::Template;

use crate::commands::node::create::{Language, NodeCreationError};
use crate::commands::pixi::execute_pixi;

#[derive(Template)]
#[template(path = "pixi.toml.j2")]
struct PixiTomlTemplate<'a> {
    node_name: &'a str,
    description: &'a str,
    dependencies_extra_msg: &'a str,
    channels: &'a str,
}

pub fn create_pixi_toml(
    node_path: &Path,
    node_name: &str,
    lang: Language,
    node_description: Option<&str>,
) -> Result<(), NodeCreationError> {
    let pixi_toml_path = node_path.join("pixi.toml");
    let mut file = fs::File::create(pixi_toml_path)?;

    let dependencies = match lang {
        Language::Python => vec!["python", "peppycl"],
        Language::Rust => vec!["rust"],
    };

    let dependencies_extra_msg = match lang {
        Language::Python => "# Python/Conda dependencies",
        Language::Rust => {
            "# Add system dependencies here, not Rust dependencies. Rust dependencies are added to Cargo.toml"
        }
    };

    let channels = match lang {
        Language::Python => "[\"conda-forge\"]",
        Language::Rust => "[\"conda-forge\"]",
    };

    let tasks = match lang {
        Language::Python => vec![],
        Language::Rust => vec![("start", "cargo run")],
    };

    let description = node_description.unwrap_or("A peppy node");

    let template = PixiTomlTemplate {
        node_name,
        description,
        dependencies_extra_msg,
        channels,
    };
    let pixi_content = template.render().map_err(|e| {
        NodeCreationError::DirectoryCreation(std::io::Error::other(format!(
            "Failed to render pixi template: {}",
            e
        )))
    })?;

    file.write_all(pixi_content.as_bytes())?;

    execute_pixi(
        &["add".to_string()]
            .into_iter()
            .chain(dependencies.iter().map(|s| s.to_string()))
            .collect::<Vec<_>>(),
        Some(node_path),
    );
    tasks.iter().for_each(|(name, command)| {
        execute_pixi(
            &[
                "task".to_string(),
                "add".to_string(),
                name.to_string(),
                command.to_string(),
            ],
            Some(node_path),
        );
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_python_pixi_toml() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let node_name = "test_node";
        let lang = Language::Python;
        let description = "Test node description";

        let result = create_pixi_toml(temp_dir.path(), node_name, lang, Some(description));
        assert!(result.is_ok());

        let pixi_path = temp_dir.path().join("pixi.toml");
        assert!(pixi_path.exists());

        let content = fs::read_to_string(&pixi_path).unwrap();
        assert!(content.contains(node_name));
        assert!(content.contains(description));
        assert!(content.contains("# Python/Conda dependencies"));

        let lock_path = temp_dir.path().join("pixi.lock");
        if lock_path.exists() {
            let lock_content = fs::read_to_string(lock_path).unwrap();
            assert!(lock_content.contains("python"));
            assert!(lock_content.contains("peppycl"));
        }

        assert!(!content.contains("[tasks]\nstart"));
    }

    #[test]
    fn test_create_rust_pixi_toml() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let node_name = "rust_test_node";
        let lang = Language::Rust;
        let description = "Rust test node description";

        let result = create_pixi_toml(temp_dir.path(), node_name, lang, Some(description));
        assert!(result.is_ok());

        let pixi_path = temp_dir.path().join("pixi.toml");
        assert!(pixi_path.exists());

        let content = fs::read_to_string(&pixi_path).unwrap();
        assert!(content.contains(node_name));
        assert!(content.contains(description));
        assert!(content.contains("# Add system dependencies here, not Rust dependencies"));

        let lock_path = temp_dir.path().join("pixi.lock");
        if lock_path.exists() {
            let lock_content = fs::read_to_string(lock_path).unwrap();
            assert!(lock_content.contains("rust"));
        }
    }
}
