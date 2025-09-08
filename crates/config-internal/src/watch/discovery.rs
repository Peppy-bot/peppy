use crate::consts::PEPPY_CONFIG_FILE;
use std::path::{Path, PathBuf};

/// Finds the `PEPPY_CONFIG_FILE` recursively starting at `from_dir`
pub fn find_peppy_nodes_from_dir(from_dir: impl AsRef<Path>) -> Vec<PathBuf> {
    let mut peppy_files = Vec::new();
    let from_dir = from_dir.as_ref();

    if !from_dir.is_dir() {
        return peppy_files;
    }

    let walker = walkdir::WalkDir::new(from_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok());

    for entry in walker {
        let path = entry.path();
        if path.is_file() && path.file_name() == Some(std::ffi::OsStr::new(PEPPY_CONFIG_FILE)) {
            peppy_files.push(path.to_path_buf());
        }
    }

    peppy_files
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_find_peppy_nodes_empty_dir() {
        let temp_dir = TempDir::new().unwrap();
        let result = find_peppy_nodes_from_dir(temp_dir.path());
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_find_peppy_nodes_non_existent_dir() {
        let result = find_peppy_nodes_from_dir("/path/that/does/not/exist");
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_find_peppy_nodes_file_instead_of_dir() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "test content").unwrap();

        let result = find_peppy_nodes_from_dir(&file_path);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_find_single_peppy_node() {
        let temp_dir = TempDir::new().unwrap();
        let peppy_file = temp_dir.path().join(PEPPY_CONFIG_FILE);
        fs::write(&peppy_file, "node_config: test").unwrap();

        let result = find_peppy_nodes_from_dir(temp_dir.path());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], peppy_file);
    }

    #[test]
    fn test_find_multiple_peppy_nodes_nested() {
        let temp_dir = TempDir::new().unwrap();

        // Create peppy.json5 in root
        let root_peppy = temp_dir.path().join(PEPPY_CONFIG_FILE);
        fs::write(&root_peppy, "node_config: root").unwrap();

        // Create nested directory with peppy.json5
        let nested_dir = temp_dir.path().join("nested");
        fs::create_dir(&nested_dir).unwrap();
        let nested_peppy = nested_dir.join(PEPPY_CONFIG_FILE);
        fs::write(&nested_peppy, "node_config: nested").unwrap();

        // Create deeply nested directory with peppy.json5
        let deep_dir = nested_dir.join("deep");
        fs::create_dir(&deep_dir).unwrap();
        let deep_peppy = deep_dir.join(PEPPY_CONFIG_FILE);
        fs::write(&deep_peppy, "node_config: deep").unwrap();

        let result = find_peppy_nodes_from_dir(temp_dir.path());
        assert_eq!(result.len(), 3);
        assert!(result.contains(&root_peppy));
        assert!(result.contains(&nested_peppy));
        assert!(result.contains(&deep_peppy));
    }

    #[test]
    fn test_find_peppy_nodes_ignores_other_files() {
        let temp_dir = TempDir::new().unwrap();

        // Create peppy.json5
        let peppy_file = temp_dir.path().join(PEPPY_CONFIG_FILE);
        fs::write(&peppy_file, "node: test").unwrap();

        // Create other files that should be ignored
        fs::write(temp_dir.path().join("config.yaml"), "other: config").unwrap();
        fs::write(temp_dir.path().join("peppy.toml"), "wrong extension").unwrap();
        fs::write(temp_dir.path().join("not_peppy.json5"), "not peppy").unwrap();

        let result = find_peppy_nodes_from_dir(temp_dir.path());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], peppy_file);
    }

    #[test]
    fn test_find_peppy_nodes_does_not_follow_symlinks() {
        let temp_dir = TempDir::new().unwrap();
        let external_dir = TempDir::new().unwrap();

        // Create peppy.json5 in external directory
        let external_peppy = external_dir.path().join(PEPPY_CONFIG_FILE);
        fs::write(&external_peppy, "node: external").unwrap();

        // Create symlink to external directory
        let symlink_path = temp_dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(external_dir.path(), &symlink_path).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(external_dir.path(), &symlink_path).unwrap();

        // Create peppy.json5 in main directory
        let main_peppy = temp_dir.path().join(PEPPY_CONFIG_FILE);
        fs::write(&main_peppy, "node: main").unwrap();

        let result = find_peppy_nodes_from_dir(temp_dir.path());
        // Should only find the main peppy.json5, not the one through symlink
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], main_peppy);
    }

    #[test]
    fn test_find_peppy_nodes_handles_permissions() {
        // This test is platform-specific and may need adjustment
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let temp_dir = TempDir::new().unwrap();

            // Create accessible peppy.json5
            let accessible_peppy = temp_dir.path().join(PEPPY_CONFIG_FILE);
            fs::write(&accessible_peppy, "node: accessible").unwrap();

            // Create directory with restricted permissions
            let restricted_dir = temp_dir.path().join("restricted");
            fs::create_dir(&restricted_dir).unwrap();
            let restricted_peppy = restricted_dir.join(PEPPY_CONFIG_FILE);
            fs::write(&restricted_peppy, "node: restricted").unwrap();

            // Remove read permissions from the directory
            let mut perms = fs::metadata(&restricted_dir).unwrap().permissions();
            perms.set_mode(0o000);
            fs::set_permissions(&restricted_dir, perms).unwrap();

            let result = find_peppy_nodes_from_dir(temp_dir.path());

            // Restore permissions for cleanup
            let mut perms = fs::metadata(&restricted_dir).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&restricted_dir, perms).unwrap();

            // Should only find the accessible one
            assert!(result.len() == 1);
            assert!(result.contains(&accessible_peppy));
        }
    }
}
