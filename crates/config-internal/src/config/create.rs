use askama::Template;
use tracing::info;

use crate::error::{Error, Result};
use std::io::Write;
use std::path::PathBuf;
use std::{fs, path::Path};

#[derive(Template)]
#[template(path = "peppy_new_node.star.j2")]
struct PeppyNodeTemplate<'a> {
    name: &'a str,
}

#[derive(Template)]
#[template(path = "init.star.j2")]
struct InitStarTemplate;

pub fn create_peppy_node_config(node_path: &Path, node_name: &str) -> Result<()> {
    let peppy_star_path = node_path.join("peppy.star");
    let mut file = fs::File::create(peppy_star_path)?;

    let template = PeppyNodeTemplate { name: node_name };
    let peppy_content = template
        .render()
        .map_err(|e| Error::AskamaError(e.to_string()))?;

    file.write_all(peppy_content.as_bytes())?;

    Ok(())
}

pub fn init_root_node(path: &Path) -> Result<PathBuf> {
    // Create the directory if it doesn't exist
    fs::create_dir_all(path)?;

    let peppy_star_path = path.join("peppy.star");
    let template = InitStarTemplate {};
    let default_content = template
        .render()
        .map_err(|e| std::io::Error::other(format!("Template error: {}", e)))?;

    fs::write(&peppy_star_path, default_content)?;

    // TODO: Must also install the systemd service in the OS if it's not already the case"
    info!("Created root node at {}", peppy_star_path.display());
    Ok(peppy_star_path)
}

#[cfg(test)]
mod tests {
    use starlark::{
        environment::{Globals, Module},
        eval::Evaluator,
        syntax::{AstModule, Dialect},
    };

    use super::*;

    #[test]
    fn test_init_root_node() {
        use starlark::environment::{Globals, Module};
        use starlark::eval::Evaluator;
        use starlark::syntax::{AstModule, Dialect};
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let non_existent_path = temp_dir.path().join("new_folder");

        assert!(!non_existent_path.exists());

        let peppy_star_path = init_root_node(&non_existent_path).unwrap();

        assert!(non_existent_path.exists());
        assert!(peppy_star_path.exists());
        assert_eq!(peppy_star_path.file_name().unwrap(), "peppy.star");

        let content = fs::read_to_string(&peppy_star_path).unwrap();
        assert!(content.contains("def create_root_node()"));
        assert!(content.contains("namespace = \"/\""));
        assert!(content.contains("qos_profile = \"default\""));

        // Validate that the generated file is valid Starlark syntax
        let ast = AstModule::parse("peppy.star", content.to_owned(), &Dialect::Extended);
        assert!(
            ast.is_ok(),
            "Generated peppy.star file should be valid Starlark syntax"
        );

        // Also evaluate it to ensure it's not just syntactically valid but also executable
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

    // Can be run from the command line with:
    // cargo run --manifest-path <path_to_root_Cargo.toml> -- node create my_project
    #[test]
    fn test_create_peppy_config() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let node_name = "test_node";

        let result = create_peppy_node_config(temp_dir.path(), &node_name);
        assert!(result.is_ok());

        let peppy_path = temp_dir.path().join("peppy.star");
        assert!(peppy_path.exists());

        let content = fs::read_to_string(&peppy_path).unwrap();
        assert!(content.contains(node_name));

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
}
