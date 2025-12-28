use config::consts::NODE_CONFIG_FILE;
use generator::RustGenerator;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;
use std::{fs, thread, time::Duration};
use tempfile::TempDir;

pub const STUB_NODE_CONFIG: &str = r#"{
  schema_version: 1,
  manifest: {
    name: "generated_node",
    tag: "0.1.0",
    launch_cmd: ["cargo", "run", "--release"]
  },
  logging: {
    min_level: "info",
    format: "text"
  }
}
"#;

pub fn prepare_directories(
    temp_dir: &TempDir,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let output_dir = temp_dir.path().join(".peppy/libs/peppygen");
    let user_node = temp_dir.path().join("user_node");
    let peppy_node_config = user_node.join(NODE_CONFIG_FILE);
    fs::create_dir_all(&output_dir).unwrap();
    fs::create_dir_all(&user_node).unwrap();
    fs::write(&peppy_node_config, STUB_NODE_CONFIG).unwrap();
    (output_dir, user_node, peppy_node_config)
}

pub fn init_test_env(
    temp_dir: &TempDir,
) -> (
    RustGenerator,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let (output_dir, user_node, peppy_node_config_path) = prepare_directories(temp_dir);
    (
        RustGenerator::new(),
        output_dir,
        user_node,
        peppy_node_config_path,
    )
}

pub fn copy_config_to_output(user_node: &Path, output_dir: &Path) -> std::path::PathBuf {
    let source = user_node.join(NODE_CONFIG_FILE);
    let destination = output_dir.join(NODE_CONFIG_FILE);
    fs::copy(&source, &destination).unwrap();
    destination
}

pub fn init_cargo_user_node(to_dir: impl AsRef<Path>) {
    let crate_dir = to_dir.as_ref();
    fs::create_dir_all(crate_dir).expect("failed to create user node directory");
    let cargo_toml_path = crate_dir.join("Cargo.toml");

    if !cargo_toml_path.exists() {
        Command::new("cargo")
            .arg("init")
            .arg("--bin")
            .arg("--vcs")
            .arg("none")
            .current_dir(crate_dir)
            .output()
            .expect("failed to invoke cargo init for user node");
        Command::new("cargo")
            .arg("add")
            .arg("tokio")
            .current_dir(crate_dir)
            .output()
            .expect("failed to invoke cargo init for user node");
    }

    let peppygen_path = "../.peppy/libs/peppygen";

    let manifest_contents =
        fs::read_to_string(&cargo_toml_path).expect("failed to read user node Cargo.toml");

    if !manifest_contents
        .lines()
        .any(|line| line.trim_start().starts_with("peppygen"))
    {
        let dependency_line = format!("peppygen = {{ path = \"{}\" }}\n", peppygen_path);
        let updated_manifest = insert_dependency_line(&manifest_contents, &dependency_line);
        fs::write(&cargo_toml_path, updated_manifest)
            .expect("failed to write user node Cargo.toml");
    }
}

pub fn spawn_cargo_run(dir: &std::path::Path, env_vars: &[(&str, &str)]) -> std::process::Child {
    let mut command = Command::new("cargo");
    command
        .arg("run")
        .env("CARGO_NET_OFFLINE", "true")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(dir);

    for &(key, value) in env_vars {
        command.env(key, value);
    }

    command.spawn().expect("failed to spawn cargo run")
}

pub fn wait_for_child(
    child: &mut std::process::Child,
    timeout: Option<Duration>,
    dir: &std::path::Path,
) -> std::process::Output {
    let start = Instant::now();
    loop {
        if let Some(limit) = timeout {
            if start.elapsed() > limit {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "cargo run timed out after {:?} for project at {}",
                    limit,
                    dir.display()
                );
            }
        }

        if let Some(status) = child
            .try_wait()
            .expect("failed to poll cargo run status for generated project")
        {
            let mut stdout = Vec::new();
            if let Some(mut out) = child.stdout.take() {
                out.read_to_end(&mut stdout)
                    .expect("failed to capture cargo stdout");
            }
            let mut stderr = Vec::new();
            if let Some(mut err) = child.stderr.take() {
                err.read_to_end(&mut stderr)
                    .expect("failed to capture cargo stderr");
            }
            return std::process::Output {
                status,
                stdout,
                stderr,
            };
        }

        thread::sleep(Duration::from_millis(50));
    }
}

pub fn run_cargo_run(
    dir: &std::path::Path,
    timeout: Option<Duration>,
    env_vars: &[(&str, &str)],
) -> std::process::Output {
    let mut child = spawn_cargo_run(dir, env_vars);
    wait_for_child(&mut child, timeout, dir)
}

pub fn compile_project(dir: impl AsRef<Path>) {
    let cargo_output = Command::new("cargo")
        .arg("build")
        .env("CARGO_NET_OFFLINE", "true")
        .current_dir(dir)
        .output()
        .expect("failed to invoke cargo build on generated crate");
    assert!(
        cargo_output.status.success(),
        "cargo build failed for generated crate with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        cargo_output.status.code(),
        String::from_utf8_lossy(&cargo_output.stdout),
        String::from_utf8_lossy(&cargo_output.stderr)
    );
}

pub fn insert_dependency_line(contents: &str, dependency_line: &str) -> String {
    let header = "[dependencies]";
    if let Some(section_start) = contents.find(header) {
        let after_header = contents[section_start..]
            .find('\n')
            .map(|offset| section_start + offset + 1)
            .unwrap_or(contents.len());
        let insert_pos = contents[after_header..]
            .find("\n[")
            .map(|offset| after_header + offset)
            .unwrap_or(contents.len());

        let mut updated = contents.to_string();
        if insert_pos > 0 && !updated[..insert_pos].ends_with('\n') {
            updated.insert(insert_pos, '\n');
        }
        updated.insert_str(insert_pos, dependency_line);
        updated
    } else {
        let mut updated = contents.to_string();
        if !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str("[dependencies]\n");
        updated.push_str(dependency_line);
        updated
    }
}

pub fn start_router_for_tests(
    rt: &tokio::runtime::Runtime,
) -> (pmi::Messenger, TempDir, String, u16) {
    rt.block_on(peppylib::start_zenohd_process("127.0.0.1", None))
        .expect("failed to start zenoh router for test")
}
