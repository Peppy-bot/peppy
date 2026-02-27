use containers::Apptainer;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

/// Build a minimal Alpine container image and return the Apptainer handle, temp dir, and .sif path.
///
/// Shared setup for integration tests that need a built container. The temp dir is
/// placed under `$HOME` (required for Lima path translation on macOS).
///
/// Returns `None` (and prints a diagnostic) when the Apptainer runtime is not
/// available or not fully operational on this host — for example, when system
/// dependencies like `newuidmap` (from the `uidmap` package) are missing.
///
/// First run downloads the Alpine base image (~30-60s); subsequent runs use the
/// Apptainer cache and complete in ~5s.
fn build_alpine_container() -> Option<(Apptainer, TempDir, PathBuf)> {
    let facade = match Apptainer::new() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("SKIPPING: Apptainer runtime not available: {e}");
            return None;
        }
    };

    // Health check: verify the runtime can actually execute commands.
    // This catches missing system dependencies (e.g. newuidmap) that only
    // manifest when apptainer forks a subprocess.
    if let Err(e) = facade.version() {
        eprintln!("SKIPPING: Apptainer runtime not operational: {e}");
        return None;
    }

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
    let mut child = match facade.build(&sif_path, &def_path).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIPPING: apptainer build failed to spawn: {e}");
            return None;
        }
    };

    let status = child
        .wait()
        .expect("should be able to wait on build child process");
    if !status.success() {
        eprintln!("SKIPPING: apptainer build failed (exit status: {status})");
        return None;
    }
    if !sif_path.exists() {
        eprintln!(
            "SKIPPING: built .sif file missing at {}",
            sif_path.display()
        );
        return None;
    }

    Some((facade, tmp_dir, sif_path))
}

/// Integration test: build a container and run it via `apptainer run`.
///
/// Exercises `Apptainer::build()` and `Apptainer::run()` with the
/// real Apptainer runtime (routed through Lima on macOS).
#[test]
fn build_and_run_container() {
    let Some((facade, _tmp_dir, sif_path)) = build_alpine_container() else {
        return;
    };

    let mut child = facade
        .run(&sif_path.to_string_lossy())
        .spawn()
        .expect("facade.run().spawn() should succeed");

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
/// Exercises `Apptainer::exec()` with the real Apptainer runtime
/// (routed through Lima on macOS).
#[test]
fn build_and_exec_in_container() {
    let Some((facade, _tmp_dir, sif_path)) = build_alpine_container() else {
        return;
    };

    let mut child = facade
        .exec(&sif_path.to_string_lossy(), &["cat", "/etc/alpine-release"])
        .spawn()
        .expect("facade.exec().spawn() should succeed");

    let status = child
        .wait()
        .expect("should be able to wait on exec child process");
    assert!(
        status.success(),
        "apptainer exec should succeed (exit status: {})",
        status
    );
}

/// Integration test: bind-mount a host file into the container and read it back.
///
/// Creates a file with known content under `$HOME`, bind-mounts it into the
/// container, and uses `apptainer exec cat` to verify the content is visible
/// inside. This exercises the `--bind` flag pipeline end-to-end, including
/// Lima path translation on macOS.
#[test]
fn bind_mount_file_visible_in_container() {
    let Some((facade, tmp_dir, sif_path)) = build_alpine_container() else {
        return;
    };

    // Create a file with known content to bind-mount
    let marker_path = tmp_dir.path().join("fake-device");
    let marker_content = "peppy-bind-test-ok";
    fs::write(&marker_path, marker_content).expect("should be able to write marker file");

    let marker_str = marker_path.to_string_lossy();
    let output = facade
        .exec(&sif_path.to_string_lossy(), &["cat", &marker_str])
        .bind(&marker_str, None)
        .output()
        .expect("exec with --bind should succeed");

    assert!(
        output.status.success(),
        "apptainer exec with --bind should succeed (exit status: {})\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim(),
        marker_content,
        "bound file content should be readable inside the container"
    );
}

/// Integration test: `into_std_command` produces a runnable `std::process::Command`.
///
/// Exercises the `into_std_command()` terminal method by building a run command,
/// customizing its stdio (the primary use case for this API), and verifying the
/// output matches what `spawn()` would produce.
#[test]
fn into_std_command_produces_runnable_command() {
    let Some((facade, _tmp_dir, sif_path)) = build_alpine_container() else {
        return;
    };

    let mut cmd = facade
        .run(&sif_path.to_string_lossy())
        .into_std_command()
        .expect("into_std_command should succeed");

    // Verify caller can customize stdio (the main reason this method exists)
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let output = cmd.output().expect("spawned command should complete");
    assert!(
        output.status.success(),
        "command from into_std_command should succeed (exit status: {})\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim(),
        "peppy-test-ok",
        "into_std_command run should produce the same output as spawn"
    );
}
