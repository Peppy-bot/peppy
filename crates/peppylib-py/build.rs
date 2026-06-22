use std::path::PathBuf;
use std::process::Command;

fn main() {
    build_helpers_shared::embed_git_tag();

    check_pixi_installed();
    // pixi is installed, configure Python path if not already set
    configure_python_from_pixi();
    check_uv_installed();

    pyo3_build_config::add_extension_module_link_args();
}

fn check_pixi_installed() {
    // Check if pixi is installed
    let pixi_check = Command::new("pixi").arg("--version").output();

    match pixi_check {
        Ok(output) if output.status.success() => {}
        _ => {
            panic!(
                r#"
================================================================================
ERROR: pixi is not installed or not found in PATH

pixi is required to build the Python bindings for peppylib.

To install pixi, run:

    curl -fsSL https://pixi.sh/install.sh | bash

For more information, visit: https://pixi.sh
================================================================================
"#
            );
        }
    }
}

fn check_uv_installed() {
    // Check if uv is installed
    let uv_check = Command::new("uv").arg("--version").output();

    match uv_check {
        Ok(output) if output.status.success() => {}
        _ => {
            panic!(
                r#"
================================================================================
ERROR: uv is not installed or not found in PATH

uv is required to build the Python bindings for peppylib.

To install uv, run:

    curl -LsSf https://astral.sh/uv/install.sh | sh

For more information, visit: https://docs.astral.sh/uv/
================================================================================
"#
            );
        }
    }
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

    // Serialize concurrent pixi invocations to avoid "Text file busy" races
    // when multiple build scripts run pixi on the same environment.
    let lock_path = manifest_dir.join(".pixi/.build.lock");
    let _pixi_lock = build_helpers_shared::acquire_file_lock(&lock_path);

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
