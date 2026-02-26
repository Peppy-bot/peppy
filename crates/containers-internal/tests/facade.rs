use containers::ApptainerFacade;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

/// Build a minimal Alpine container image and return the facade, temp dir, and .sif path.
///
/// Shared setup for integration tests that need a built container. The temp dir is
/// placed under `$HOME` (required for Lima path translation on macOS).
///
/// First run downloads the Alpine base image (~30-60s); subsequent runs use the
/// Apptainer cache and complete in ~5s.
fn build_alpine_container() -> (ApptainerFacade, TempDir, PathBuf) {
    let facade = ApptainerFacade::new()
        .expect("ApptainerFacade::new() should succeed — apptainer is bundled at compile time");

    let home = std::env::var("HOME").expect("HOME environment variable must be set");
    let test_tmp_root = PathBuf::from(&home).join(".peppy/test-tmp");
    fs::create_dir_all(&test_tmp_root).expect("should be able to create ~/.peppy/test-tmp/");
    let tmp_dir = TempDir::new_in(&test_tmp_root)
        .expect("should be able to create temp dir under ~/.peppy/test-tmp/");

    let def_path = tmp_dir.path().join("test.def");
    fs::write(
        &def_path,
        "\
Bootstrap: docker
From: alpine:3.20

%runscript
    echo peppy-test-ok
",
    )
    .expect("should be able to write .def file");

    let sif_path = tmp_dir.path().join("test.sif");
    let mut child = facade
        .build(&sif_path, &def_path)
        .expect("facade.build() should spawn successfully");

    let status = child
        .wait()
        .expect("should be able to wait on build child process");
    assert!(
        status.success(),
        "apptainer build should succeed (exit status: {})",
        status
    );
    assert!(
        sif_path.exists(),
        "built .sif file should exist at {}",
        sif_path.display()
    );

    (facade, tmp_dir, sif_path)
}

/// Integration test: build a container and run it via `apptainer run`.
///
/// Exercises `ApptainerFacade::build()` and `ApptainerFacade::run()` with the
/// real Apptainer runtime (routed through Lima on macOS).
#[test]
fn build_and_run_container() {
    let (facade, _tmp_dir, sif_path) = build_alpine_container();

    let mut child = facade
        .run(&sif_path.to_string_lossy(), &[])
        .expect("facade.run() should spawn successfully");

    let status = child
        .wait()
        .expect("should be able to wait on run child process");
    assert!(
        status.success(),
        "apptainer run should succeed (exit status: {})",
        status
    );
}

/// Integration test: build a container and execute a command inside it via `apptainer exec`.
///
/// Exercises `ApptainerFacade::exec()` with the real Apptainer runtime
/// (routed through Lima on macOS).
#[test]
fn build_and_exec_in_container() {
    let (facade, _tmp_dir, sif_path) = build_alpine_container();

    let mut child = facade
        .exec(&sif_path.to_string_lossy(), &["cat", "/etc/alpine-release"])
        .expect("facade.exec() should spawn successfully");

    let status = child
        .wait()
        .expect("should be able to wait on exec child process");
    assert!(
        status.success(),
        "apptainer exec should succeed (exit status: {})",
        status
    );
}
