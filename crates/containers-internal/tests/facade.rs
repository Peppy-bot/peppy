use config::consts::DEFAULT_ALPINE_BASE_IMAGE;
use containers::Apptainer;
use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use tempfile::TempDir;

/// Build a minimal Alpine container image whose `%runscript` echoes
/// `peppy-test-ok`, returning the Apptainer handle, temp dir, and .sif path.
///
/// Thin wrapper over [`build_container_with_runscript`] for tests that just need
/// a runnable container.
fn build_alpine_container() -> Option<(Apptainer, TempDir, PathBuf)> {
    build_container_with_runscript("    echo peppy-test-ok\n")
}

/// Build a minimal Alpine container image with a custom `%runscript` body and
/// return the Apptainer handle, temp dir, and .sif path.
///
/// Shared setup for integration tests that need a built container. The temp dir is
/// placed under `$HOME` (required for Lima path translation on macOS). The
/// `runscript_body` is spliced verbatim under `%runscript` (callers supply their
/// own indentation and trailing newline).
///
/// Returns `None` (and prints a diagnostic) when the Apptainer runtime is not
/// available or not fully operational on this host — for example, when system
/// dependencies like `newuidmap` (from the `uidmap` package) are missing.
///
/// First run downloads the Alpine base image (~30-60s); subsequent runs use the
/// Apptainer cache and complete in ~5s.
fn build_container_with_runscript(runscript_body: &str) -> Option<(Apptainer, TempDir, PathBuf)> {
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

    let tmp_dir = TempDir::new_in(config::test_helpers::test_tmp_root())
        .expect("should be able to create temp dir under the shared test-tmp root");

    let def_path = tmp_dir.path().join("test.def");
    fs::write(
        &def_path,
        format!(
            "Bootstrap: docker\n\
             From: {DEFAULT_ALPINE_BASE_IMAGE}\n\
             \n\
             %runscript\n\
             {runscript_body}"
        ),
    )
    .expect("should be able to write .def file");

    let sif_path = tmp_dir.path().join("test.sif");

    let mut cmd = match facade.build(&sif_path, &def_path).into_std_command() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIPPING: apptainer build failed to create command: {e}");
            return None;
        }
    };
    cmd.stdin(Stdio::null());
    let mut child = match cmd.spawn() {
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

    let mut cmd = facade
        .run(&sif_path.to_string_lossy())
        .into_std_command()
        .expect("facade.run().into_std_command() should succeed");
    cmd.stdin(Stdio::null());

    let mut child = cmd.spawn().expect("run command should spawn");
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

    let mut cmd = facade
        .exec(&sif_path.to_string_lossy(), &["cat", "/etc/alpine-release"])
        .into_std_command()
        .expect("facade.exec().into_std_command() should succeed");
    cmd.stdin(Stdio::null());

    let mut child = cmd.spawn().expect("exec command should spawn");
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
    let mut cmd = facade
        .exec(&sif_path.to_string_lossy(), &["cat", &marker_str])
        .bind(&marker_str, None, None)
        .into_std_command()
        .expect("exec with --bind should build command");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let output = cmd.output().expect("exec with --bind should succeed");

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
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

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

/// Polls `cond` until it returns `true` or `timeout` elapses. Returns the final
/// value of `cond`.
#[cfg(target_os = "macos")]
fn poll_until(timeout: std::time::Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    cond()
}

/// `true` if any process inside the Lima guest matches `marker` (full-command
/// `pgrep -f`). `pgrep` exits 0 when at least one process matches, 1 otherwise.
/// Panics when the `pgrep` invocation itself cannot run, so a broken guest
/// channel fails the test immediately instead of reading as "not found".
#[cfg(target_os = "macos")]
fn guest_has_process(facade: &Apptainer, marker: &str) -> bool {
    facade
        .guest_command(&["pgrep", "-f", marker])
        .expect("guest_command (pgrep) should run")
        .status
        .success()
}

/// Kills and reaps the wrapped child on drop so a failing assertion never
/// leaks the spawned host process.
#[cfg(target_os = "macos")]
struct ChildGuard(std::process::Child);

#[cfg(target_os = "macos")]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Integration test (macOS + Lima): a container `run` wrapped with `cancel_pgid`
/// records its in-VM process group, and `kill_guest_process_group` (same key)
/// SIGKILLs that whole group immediately.
///
/// This is the in-VM half of the daemon's deliberate-stop teardown: on macOS a
/// host process-group kill only reaches the `limactl` client, so a non-responsive
/// container node's workload lives on inside the Lima VM until this guest-side
/// kill reaps it — without waiting for the in-container daemon watchdog's grace
/// period. The runscript `exec sleep`s on a long, distinctive duration so the
/// only guest process matching that marker is the container workload itself (not
/// the wrapper, apptainer, or kill argv).
///
/// macOS-only: under the native (Linux) backend the wrapper is a no-op and the
/// shared-namespace container is reaped by the host process-group kill instead,
/// so the guest-side kill does not apply.
#[cfg(target_os = "macos")]
#[test]
fn cancel_pgid_run_is_killable_in_guest() {
    use std::time::Duration;

    // Distinctive sleep duration: appears only in the actual in-VM `sleep`
    // process, never in the wrapper/apptainer/kill command lines.
    const SLEEP_MARKER: &str = "524287";
    // Filesystem-safe key, mirroring an instance id. Spawn and teardown must use
    // the same key; here both sides are this constant.
    const INSTANCE_KEY: &str = "teardown-killtest-instance";

    let Some((facade, _tmp_dir, sif_path)) =
        build_container_with_runscript(&format!("    exec sleep {SLEEP_MARKER}\n"))
    else {
        return;
    };

    // Sanity: the workload is not already running under this marker.
    assert!(
        !guest_has_process(&facade, SLEEP_MARKER),
        "no stray '{SLEEP_MARKER}' process should exist in the guest before the run"
    );

    let mut cmd = facade
        .run(&sif_path.to_string_lossy())
        .cancel_pgid(INSTANCE_KEY)
        .into_std_command()
        .expect("run().cancel_pgid().into_std_command() should succeed");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    // Guarded so the host `limactl shell` child is reaped even if an assertion
    // below panics.
    let _child = ChildGuard(cmd.spawn().expect("run command should spawn"));

    // The in-VM workload should come up (first run may pull layers/start slowly).
    assert!(
        poll_until(Duration::from_secs(60), || guest_has_process(
            &facade,
            SLEEP_MARKER
        )),
        "the in-VM container workload should be running before the kill"
    );

    // System under test: reach into the VM and SIGKILL the recorded group.
    facade
        .kill_guest_process_group(INSTANCE_KEY)
        .expect("guest kill should not error");

    // It must be gone promptly — proving the deliberate-stop path does not wait
    // for the in-container watchdog grace period.
    assert!(
        poll_until(Duration::from_secs(10), || !guest_has_process(
            &facade,
            SLEEP_MARKER
        )),
        "kill_guest_process_group must SIGKILL the in-VM workload immediately"
    );
}
