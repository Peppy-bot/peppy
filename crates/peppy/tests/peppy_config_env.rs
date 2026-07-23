//! Real-binary coverage for the daemon-config environment override.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const INVALID_CONFIG: &str = "{ not json5";
const PROCESS_TIMEOUT: Duration = Duration::from_secs(30);

fn run_with_invalid_override(home: &Path, args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_peppy"))
        .args(args)
        .current_dir(home)
        .env(config::consts::PEPPY_HOME_ENV, home)
        .env(config::consts::PEPPY_CONFIG_ENV, INVALID_CONFIG)
        .env("RUST_LOG", "error")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn peppy");

    let start = Instant::now();
    loop {
        if child.try_wait().expect("failed to poll peppy").is_some() {
            return child
                .wait_with_output()
                .expect("failed to collect peppy output");
        }
        if start.elapsed() >= PROCESS_TIMEOUT {
            child.kill().expect("failed to kill timed-out peppy");
            let output = child
                .wait_with_output()
                .expect("failed to collect timed-out peppy output");
            panic!(
                "peppy did not exit within {PROCESS_TIMEOUT:?}; {}",
                combined_output(&output)
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn combined_output(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

#[test]
fn serve_with_invalid_peppy_config_env_exits_one_naming_the_var() {
    let home = tempfile::tempdir().expect("temp home");
    let output = run_with_invalid_override(
        home.path(),
        &["service", "serve", "--messaging-engine", "mock"],
    );
    let combined = combined_output(&output);

    assert_eq!(
        output.status.code(),
        Some(1),
        "unexpected status; {combined}"
    );
    assert!(
        combined.contains(config::consts::PEPPY_CONFIG_ENV),
        "error did not name PEPPY_CONFIG; {combined}"
    );
    assert!(
        !home.path().join("conf/peppy_config.json5").exists(),
        "override mode created the normal config file"
    );
}

#[test]
fn platform_whoami_with_invalid_peppy_config_env_exits_one() {
    let home = tempfile::tempdir().expect("temp home");
    let output = run_with_invalid_override(home.path(), &["platform", "whoami"]);
    let combined = combined_output(&output);

    assert_eq!(
        output.status.code(),
        Some(1),
        "unexpected status; {combined}"
    );
    assert!(
        combined.contains(config::consts::PEPPY_CONFIG_ENV),
        "error did not name PEPPY_CONFIG; {combined}"
    );
}
