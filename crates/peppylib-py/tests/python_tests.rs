//! Integration test that runs Python pytest suite.

use std::process::Command;

#[test]
fn run_pytest_suite() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");

    let output = Command::new("pixi")
        .args(["run", "test"])
        .current_dir(manifest_dir)
        .output()
        .expect("Failed to execute pixi run test. Is pixi installed and environment set up?");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    println!("{stdout}");
    if !stderr.is_empty() {
        eprintln!("{stderr}");
    }

    assert!(
        output.status.success(),
        "Python tests failed with exit code: {:?}",
        output.status.code()
    );
}
