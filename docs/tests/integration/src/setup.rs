use std::ffi::OsString;
use std::path::{Path, PathBuf};
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
        let target_dir = cargo_target_dir(&workspace, std::env::var_os("CARGO_TARGET_DIR"));

        // The build is told its target directory, so the binary is looked up
        // where it was built even when a cargo config or `CARGO_BUILD_TARGET_DIR`
        // moves the default one.
        let status = Command::new("cargo")
            .arg("build")
            .arg("--release")
            .arg("-p")
            .arg("peppy")
            .arg("--target-dir")
            .arg(&target_dir)
            .current_dir(&workspace)
            .status()
            .expect("failed to execute cargo build");

        assert!(status.success(), "cargo build --release -p peppy failed");

        let binary_path = target_dir.join("release").join("peppy");
        assert!(
            binary_path.exists(),
            "peppy binary not found at {}",
            binary_path.display()
        );

        binary_path
    })
}

/// The target directory of the peppy build: the `CARGO_TARGET_DIR` of the test
/// run when it is set and not empty, so that a run with a private target
/// directory builds peppy there as every other cargo command of that run does,
/// else the workspace `target/`. A relative directory resolves against the
/// workspace, where the build runs.
fn cargo_target_dir(workspace: &Path, target_dir_override: Option<OsString>) -> PathBuf {
    match target_dir_override {
        Some(dir) if !dir.is_empty() => workspace.join(dir),
        _ => workspace.join("target"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_target_dir_is_the_workspace_target_without_an_override() {
        let workspace = Path::new("/ws");

        assert_eq!(cargo_target_dir(workspace, None), Path::new("/ws/target"));
        assert_eq!(
            cargo_target_dir(workspace, Some(OsString::new())),
            Path::new("/ws/target")
        );
    }

    #[test]
    fn cargo_target_dir_takes_an_absolute_override_as_is() {
        let target_dir = cargo_target_dir(Path::new("/ws"), Some(OsString::from("/private/t")));

        assert_eq!(target_dir, Path::new("/private/t"));
    }

    #[test]
    fn cargo_target_dir_resolves_a_relative_override_against_the_workspace() {
        let target_dir = cargo_target_dir(Path::new("/ws"), Some(OsString::from("rel/t")));

        assert_eq!(target_dir, Path::new("/ws/rel/t"));
    }
}
