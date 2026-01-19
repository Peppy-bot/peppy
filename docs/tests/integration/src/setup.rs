use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// Path to the workspace root (where the main Cargo.toml is)
pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Compiles the peppy binary in release mode and returns the path to the executable.
/// The compilation only happens once per test run.
pub fn peppy_binary() -> &'static PathBuf {
    static BINARY_PATH: OnceLock<PathBuf> = OnceLock::new();

    BINARY_PATH.get_or_init(|| {
        let workspace = workspace_root();

        let status = Command::new("cargo")
            .arg("build")
            .arg("--release")
            .arg("-p")
            .arg("peppy")
            .current_dir(&workspace)
            .status()
            .expect("failed to execute cargo build");

        assert!(status.success(), "cargo build --release -p peppy failed");

        let binary_path = workspace.join("target/release/peppy");
        assert!(
            binary_path.exists(),
            "peppy binary not found at {}",
            binary_path.display()
        );

        binary_path
    })
}
