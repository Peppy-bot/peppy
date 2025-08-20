use anyhow::Result;
use std::fs;
use std::io::Write;
use std::path::Path;

use askama::Template;

use crate::commands::node::types::{Language, NodeName};
use crate::commands::pixi::PixiFacade;

#[derive(Template)]
#[template(path = "pixi.toml.j2")]
struct PixiTomlTemplate<'a> {
    node_name: &'a str,
    description: &'a str,
    dependencies_extra_msg: &'a str,
    channels: &'a str,
}

/// Language-specific configuration for pixi.toml
struct LanguageConfig {
    dependencies: Vec<&'static str>,
    dependencies_msg: &'static str,
    channels: &'static str,
    tasks: Vec<(&'static str, &'static str)>,
    default_description_suffix: &'static str,
}

impl LanguageConfig {
    fn for_language(lang: Language) -> Self {
        match lang {
            Language::Python => Self {
                dependencies: vec!["python", "peppycl"],
                dependencies_msg: "# Python/Conda dependencies",
                channels: r#"["https://repo.prefix.dev/peppy", "conda-forge"]"#,
                tasks: vec![],
                default_description_suffix: "Python",
            },
            Language::Rust => Self {
                dependencies: vec!["rust"],
                dependencies_msg: "# Add system dependencies here, not Rust dependencies. Rust dependencies are added to Cargo.toml",
                channels: r#"["conda-forge"]"#,
                tasks: vec![("build", "cargo build"), ("start", "cargo run")],
                default_description_suffix: "Rust",
            },
        }
    }
}

pub fn create_pixi_toml(
    node_path: &Path,
    node_name: &NodeName,
    lang: Language,
    node_description: Option<&str>,
) -> Result<()> {
    let config = LanguageConfig::for_language(lang);

    let pixi_toml_path = node_path.join("pixi.toml");
    let mut file = fs::File::create(&pixi_toml_path)?;

    let default_description = format!(
        "{} Peppy {} node",
        node_name.as_str(),
        config.default_description_suffix
    );
    let description = node_description.unwrap_or(&default_description);

    let template = PixiTomlTemplate {
        node_name: node_name.as_str(),
        description,
        dependencies_extra_msg: config.dependencies_msg,
        channels: config.channels,
    };

    let pixi_content = template.render()?;
    file.write_all(pixi_content.as_bytes())?;

    // Create PixiFacade instance with node path as working directory
    let pixi = PixiFacade::new(node_path.to_path_buf())?;

    // Install dependencies
    pixi.install()?;

    // Add dependencies
    pixi.add_dependencies(&config.dependencies)?;

    // Add tasks
    for (name, command) in config.tasks {
        pixi.add_task(name, command)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_python_pixi_toml() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let node_name = NodeName::new("test_node").unwrap();
        let lang = Language::Python;
        let description = "Test node description";

        let result = create_pixi_toml(temp_dir.path(), &node_name, lang, Some(description));
        assert!(result.is_ok());

        let pixi_path = temp_dir.path().join("pixi.toml");
        assert!(pixi_path.exists());

        let content = fs::read_to_string(&pixi_path).unwrap();
        assert!(content.contains(node_name.as_str()));
        assert!(content.contains(description));
        assert!(content.contains("# Python/Conda dependencies"));

        let lock_path = temp_dir.path().join("pixi.lock");
        if lock_path.exists() {
            let lock_content = fs::read_to_string(lock_path).unwrap();
            assert!(lock_content.contains("python"));
            assert!(lock_content.contains("peppycl"));
        }

        assert!(content.contains("https://repo.prefix.dev/peppy"));
    }

    #[test]
    fn test_create_rust_pixi_toml() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let node_name = NodeName::new("rust_test_node").unwrap();
        let lang = Language::Rust;
        let description = "Rust test node description";

        let result = create_pixi_toml(temp_dir.path(), &node_name, lang, Some(description));
        assert!(result.is_ok());

        let pixi_path = temp_dir.path().join("pixi.toml");
        assert!(pixi_path.exists());

        let content = fs::read_to_string(&pixi_path).unwrap();
        assert!(content.contains(node_name.as_str()));
        assert!(content.contains(description));
        assert!(content.contains("# Add system dependencies here, not Rust dependencies"));

        let lock_path = temp_dir.path().join("pixi.lock");
        if lock_path.exists() {
            let lock_content = fs::read_to_string(lock_path).unwrap();
            assert!(lock_content.contains("rust"));
        }
    }
}
