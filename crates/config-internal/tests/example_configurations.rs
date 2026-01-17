use std::fs;
use std::path::{Path, PathBuf};

use config::FSNodeConfigIndex;

fn find_example_projects(base_directory: &Path) -> Vec<PathBuf> {
    let mut example_project_paths = Vec::new();

    if let Ok(directory_entries) = fs::read_dir(base_directory) {
        for entry in directory_entries.flatten() {
            let entry_path = entry.path();

            let is_example_node_directory = entry_path.is_dir()
                && entry_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("nodes_example_"));

            if is_example_node_directory {
                example_project_paths.push(entry_path);
            }
        }
    }

    example_project_paths.sort();
    example_project_paths
}

#[test]
// Uses the node configuration examples in `examples/nodes_example_*` and builds
// the node index. Each project directory is scanned
// recursively and all `peppy.json5` files are parsed. The files in
// `examples/nodes_example_*` are the ground truth; if this test fails, the
// parsing/types are out of sync with the examples.
fn test_example_project_parsing() {
    let examples_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples");
    assert!(
        examples_root.is_dir(),
        "Examples directory not found: {}",
        examples_root.display()
    );

    let projects = find_example_projects(&examples_root);
    assert!(
        !projects.is_empty(),
        "No example projects found under {}",
        examples_root.display()
    );

    for project in projects {
        // Build the aggregated index snapshot for the project directory
        let state = FSNodeConfigIndex::new(&project)
            .expect("index init")
            .into_state();

        println!("\nProject: {}", project.display());

        // Ensure at least the root peppy.json5 is discovered
        assert!(
            state
                .keys()
                .any(|p| p.ends_with(config::consts::NODE_CONFIG_FILE)),
            "No peppy.json5 discovered in project {}",
            project.display()
        );

        // Assert all discovered configs currently parse successfully
        // NOTE: If this assertion fails, it indicates the config schema in code
        // is out of sync with the example files (ground truth).
        let mut entries: Vec<_> = state.iter().collect();
        entries.sort_by_key(|(p, _)| p.display().to_string());

        println!("Found {} config file(s)", entries.len());

        for (path, result) in entries {
            match result {
                Ok(_) => println!("[OK] {}", path.display()),
                Err(err) => println!("[ERR] {}: {}", path.display(), err),
            }

            assert!(
                result.is_ok(),
                "Failed to parse {}: {:?}",
                path.display(),
                result.as_ref().err()
            );
        }
    }
}
