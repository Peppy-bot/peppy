use std::fs;
use std::path::PathBuf;

pub fn init() -> Result<PathBuf, std::io::Error> {
    let current_path = std::env::current_dir()?;
    let peppy_star_path = current_path.join("peppy.star");

    // Create the peppy.star file with default content
    let default_content = r#"def create_root_node():
    """Creates and returns the root node configuration."""
    return struct(
        namespace = "/",

        # Quality of Service settings
        qos_profile = "default",

        # Resource limits
        resources = struct(
            max_memory_mb = 512,
            cpu_affinity = [],  # CPU cores to pin to (empty = no pinning)
        ),

        # Logging configuration
        logging = struct(
            level = "info",  # "debug", "info", "warn", "error", "fatal"
            to_file = False,
            file_path = "",
        ),
    )

# Export the root node configuration
root_node = create_root_node()

# Define what this module exports when loaded
exported = struct(
    node = root_node,
)
"#;

    fs::write(&peppy_star_path, default_content)?;
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
        std::env::set_current_dir(&temp_dir).unwrap();

        let peppy_star_path = init().unwrap();

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
