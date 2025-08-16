use askama::Template;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Template)]
#[template(path = "init.star.j2")]
struct InitStarTemplate;

pub fn init(path: &Path) -> Result<PathBuf, std::io::Error> {
    let peppy_star_path = path.join("peppy.star");

    // Render the template
    let template = InitStarTemplate {};
    let default_content = template
        .render()
        .map_err(|e| std::io::Error::other(format!("Template error: {}", e)))?;

    fs::write(&peppy_star_path, default_content)?;

    // TODO: Must also install the systemd service in the OS if it's not already the case"
    Ok(peppy_star_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_peppy_config() {
        use std::fs;
        use tempfile::TempDir;

        // Create a temporary directory for testing
        let temp_dir = TempDir::new().unwrap();

        let peppy_star_path = init(temp_dir.path()).unwrap();

        // Check if peppy.star file was created at the returned path
        assert!(
            peppy_star_path.exists(),
            "peppy.star file should be created"
        );
        assert_eq!(peppy_star_path.file_name().unwrap(), "peppy.star");

        // Verify the file content
        let content = fs::read_to_string(&peppy_star_path).unwrap();
        assert!(content.contains("def create_root_node()"));
        assert!(content.contains("namespace = \"/\""));
        assert!(content.contains("qos_profile = \"default\""));
    }
}
