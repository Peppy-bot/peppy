use std::fs;
use std::path::{Path, PathBuf};

use config::NodeConfigWatcher;

fn find_example_projects(base: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                    if name.starts_with("nodes_example_") {
                        out.push(path);
                    }
                }
            }
        }
    }
    out.sort();
    out
}

#[test]
// Uses the node configuration examples in `examples/nodes_example_*` and builds
// the node index with `NodeConfigWatcher`. Each project directory is scanned
// recursively and all `peppy.yaml` files are parsed. The files in
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
        // Initialize watcher on the project directory to build the aggregated state
        let watcher = NodeConfigWatcher::new(&project).expect("watcher init");
        let rx = watcher.subscribe();
        let state = rx.borrow().clone();

        // Ensure at least the root peppy.yaml is discovered
        assert!(
            state.keys().any(|p| p.ends_with("peppy.yaml")),
            "No peppy.yaml discovered in project {}",
            project.display()
        );

        // Assert all discovered configs currently parse successfully
        // NOTE: If this assertion fails, it indicates the config schema in code
        // is out of sync with the example files (ground truth).
        for (path, result) in state.iter() {
            assert!(
                result.is_ok(),
                "Failed to parse {}: {:?}",
                path.display(),
                result.as_ref().err()
            );
        }
    }
}
