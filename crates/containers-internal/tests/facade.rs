use containers::ApptainerFacade;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

/// Integration test: build a minimal Apptainer container image from a `.def` file,
/// then run it to verify the full build-and-execute lifecycle.
///
/// This exercises `ApptainerFacade::build()` and `ApptainerFacade::run()` with the
/// real Apptainer runtime (routed through Lima on macOS).
///
/// First run downloads the Alpine base image (~30-60s); subsequent runs use the
/// Apptainer cache and complete in ~5s.
#[test]
fn build_and_run_container() {
    // 1. Create the facade (boots Lima VM on macOS if needed)
    let facade = ApptainerFacade::new()
        .expect("ApptainerFacade::new() should succeed — apptainer is bundled at compile time");

    // 2. Create a temp directory under $HOME (required for Lima path translation on macOS)
    let home = std::env::var("HOME").expect("HOME environment variable must be set");
    let test_tmp_root = PathBuf::from(&home).join(".peppy/test-tmp");
    fs::create_dir_all(&test_tmp_root).expect("should be able to create ~/.peppy/test-tmp/");
    let tmp_dir = TempDir::new_in(&test_tmp_root)
        .expect("should be able to create temp dir under ~/.peppy/test-tmp/");

    // 3. Write a minimal .def file
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

    // 4. Build the container image
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

    // 5. Run the built image to verify it works end-to-end
    let mut run_child = facade
        .run(&sif_path.to_string_lossy(), &[])
        .expect("facade.run() should spawn successfully");

    let run_status = run_child
        .wait()
        .expect("should be able to wait on run child process");
    assert!(
        run_status.success(),
        "apptainer run should succeed (exit status: {})",
        run_status
    );
}
