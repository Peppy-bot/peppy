use askama::Template;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::info;

use super::Command;
use crate::Result;

#[derive(Template)]
#[template(path = "init.star.j2")]
struct InitStarTemplate;

pub struct InitCommand {
    pub in_dir: Option<PathBuf>,
}

impl Command for InitCommand {
    fn execute(self) -> Result<()> {
        let current_dir = if let Some(in_dir) = self.in_dir {
            in_dir
        } else {
            std::env::current_dir()?
        };
        init(&current_dir)?;
        Ok(())
    }
}

pub fn init(path: &Path) -> Result<PathBuf> {
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
    use super::*;

    #[test]
    fn test_create_peppy_config() {
        use starlark::environment::{Globals, Module};
        use starlark::eval::Evaluator;
        use starlark::syntax::{AstModule, Dialect};
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let non_existent_path = temp_dir.path().join("new_folder");

        assert!(!non_existent_path.exists());

        let peppy_star_path = init(&non_existent_path).unwrap();

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
}
