use std::process::Command;

use daemon_config::consts::PEPPY_VERSION;

/// `peppy --version` is what a consumer's CI runs right after extracting a
/// release: no daemon, no state on disk, nothing but the binary. The answer
/// has to come from the binary alone, on stdout, with nothing on stderr.
#[test]
fn version_flag_answers_without_peppy_state_or_daemon() {
    let home = tempfile::tempdir().expect("temp PEPPY_HOME");
    let output = Command::new(env!("CARGO_BIN_EXE_peppy"))
        .arg("--version")
        .env("PEPPY_HOME", home.path())
        .output()
        .expect("peppy --version runs");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim_end(),
        format!("peppy {PEPPY_VERSION}")
    );
    assert!(
        output.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
