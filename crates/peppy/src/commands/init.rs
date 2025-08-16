use askama::Template;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Template)]
#[template(path = "init.star.j2")]
struct InitStarTemplate;

pub fn init(path: &Path) -> Result<PathBuf, std::io::Error> {
    // Create the directory if it doesn't exist
    fs::create_dir_all(path)?;

    let peppy_star_path = path.join("peppy.star");
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
    }
}
