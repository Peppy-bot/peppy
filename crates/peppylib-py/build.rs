use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Embed the git tag if provided (set by build_release.sh via PEPPY_GIT_TAG env var)
    if let Ok(git_tag) = std::env::var("PEPPY_GIT_TAG") {
        if !git_tag.is_empty() {
            println!("cargo:rustc-env=PEPPY_GIT_TAG={}", git_tag);
        }
    }
    println!("cargo:rerun-if-env-changed=PEPPY_GIT_TAG");

    // Check if pixi is installed
    let pixi_check = Command::new("pixi").arg("--version").output();

    match pixi_check {
        Ok(output) if output.status.success() => {
            // pixi is installed, configure Python path if not already set
            configure_python_from_pixi();
        }
        _ => {
            panic!(
                r#"
================================================================================
ERROR: pixi is not installed or not found in PATH

pixi is required to build the Python bindings for peppylib.

To install pixi, run one of the following commands:

  Linux/macOS:
    curl -fsSL https://pixi.sh/install.sh | bash

  Windows:
    powershell -ExecutionPolicy ByPass -c "irm https://pixi.sh/install.ps1 | iex"

For more information, visit: https://pixi.sh
================================================================================
"#
            );
        }
    }

    pyo3_build_config::add_extension_module_link_args();
}

fn configure_python_from_pixi() {
    // Skip if PYO3_PYTHON is already set
    if std::env::var("PYO3_PYTHON").is_ok() {
        return;
    }

    // Find the manifest path relative to this crate
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let pixi_toml = manifest_dir.join("pixi.toml");

    if !pixi_toml.exists() {
        panic!("pixi.toml not found at {:?}", pixi_toml);
    }

    // Get Python path from pixi
    let output = Command::new("pixi")
        .args(["run", "--manifest-path"])
        .arg(&pixi_toml)
        .args(["which", "python"])
        .output()
        .expect("Failed to run pixi");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!(
            "Failed to get Python path from pixi. Make sure to run 'pixi install' first.\n{}",
            stderr
        );
    }

    let python_path = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Tell cargo to rerun if pixi.lock changes
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("pixi.lock").display()
    );

    // Set PYO3_PYTHON for pyo3-build-config
    // SAFETY: build scripts run single-threaded before the main compilation
    unsafe {
        std::env::set_var("PYO3_PYTHON", &python_path);
    }
}
