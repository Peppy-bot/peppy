//! A Python node that asks for the codec API (`execution.codec_api: true`)
//! gets `peppylib.codec` and the Cap'n Proto compiler of its platform: it
//! parses a contract that the node was not built with and lays out a
//! message of it at run time, with no compiler from the environment.

use config::consts::NODE_CONFIG_FILE;
use config::node::PeppygenLanguage;
use daemon_config::consts::PEPPYLIB_OUTPUT_PATH;
use std::fs;
use std::process::Command;
use tempfile::TempDir;

use crate::helpers;

const NODE_CONFIG: &str = r#"{
  peppy_schema: "node/v1",
  manifest: { name: "codec_node", tag: "v1" },
  execution: {
    language: "python",
    run_cmd: ["uv", "run", "python", "main.py"],
    codec_api: true,
  },
}"#;

const SCRIPT: &str = r#"
from peppylib import codec

contract = codec.Contract.parse('''{
    peppy_schema: "contract/v1",
    manifest: { name: "arm", tag: "v1" },
    interfaces: {
        services: [{
            name: "home",
            request_message_format: {
                speed: "f32",
                item: { $type: "string", $marks: ["item_id"] },
            },
        }],
    },
}''')
assert contract.field_marks("service", "home", "request", ["item"]) == ["item_id"]
request = codec.MessageCodec("home_request", contract.message_format("service", "home", "request"))
value = {"speed": 0.5, "item": "cup"}
assert request.decode(request.encode(value)) == value
"#;

#[test]
fn a_python_node_that_asks_for_the_codec_api_lays_out_a_contract_at_run_time() {
    let temp_dir = TempDir::new_in(config_test_support::test_tmp_root())
        .expect("failed to create temp directory");
    let node_dir = temp_dir.path();
    let peppy_home = TempDir::new_in(config_test_support::test_tmp_root())
        .expect("failed to create temp directory");
    fs::write(node_dir.join(NODE_CONFIG_FILE), NODE_CONFIG).expect("writes peppy.json5");

    helpers::generate_test_peppygen_lib(PeppygenLanguage::Python, node_dir, Vec::new())
        .expect("the library generates");
    let package = node_dir.join(PEPPYLIB_OUTPUT_PATH).join("peppylib");
    assert!(package.join("codec.py").is_file());
    assert!(package.join("capnp_compiler").is_file());

    helpers::init_python_user_node(node_dir);
    helpers::init_python_project_venv(node_dir);
    let output = Command::new(node_dir.join(".venv/bin/python"))
        .args(["-c", SCRIPT])
        .current_dir(node_dir)
        .env_remove(peppy_codec::compiler::CAPNP_BINARY_PATH_ENV)
        .env(config::consts::PEPPY_HOME_ENV, peppy_home.path())
        .output()
        .expect("failed to run venv python");
    assert!(
        output.status.success(),
        "the codec API of the node failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let extracted: Vec<String> = fs::read_dir(peppy_home.path().join("bin"))
        .expect("the codec extracts the compiler into the bin directory of PEPPY_HOME")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(
        extracted
            .iter()
            .any(|name| name.starts_with("peppy_capnp_")),
        "{extracted:?}"
    );
}
