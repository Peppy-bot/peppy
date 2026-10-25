//! A Rust node that asks for the codec API (`execution.codec_api: true`)
//! gets `peppy-codec` with the Cap'n Proto compiler of its build target: its
//! tests parse a contract that the node was not built with and lay out a
//! message of it at run time, with no compiler from the environment.

use config::consts::NODE_CONFIG_FILE;
use config::node::PeppygenLanguage;
use std::fs;
use tempfile::TempDir;
use toml_edit::{DocumentMut, value};

use crate::helpers;

const NODE_CONFIG: &str = r#"{
  peppy_schema: "node/v1",
  manifest: { name: "codec_node", tag: "v1" },
  execution: {
    language: "rust",
    run_cmd: ["./target/debug/codec_node"],
    codec_api: true,
  },
}"#;

const NODE_MAIN: &str = r##"fn main() {}

#[cfg(test)]
mod tests {
    use peppy_codec::MessageCodec;
    use peppy_codec::contract::PeppyContractParser;

    const CONTRACT: &str = r#"{
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
    }"#;

    #[test]
    fn the_codec_lays_out_a_message_of_a_contract() {
        let contract = PeppyContractParser::from_content(CONTRACT).expect("the contract parses");
        let format = contract
            .service("home")
            .and_then(|service| service.request_message_format.as_ref())
            .expect("the request format");
        assert!(format.0["item"].marks().contains("item_id"));
        let codec = MessageCodec::new("home_request", format).expect("the format lays out");
        let value = serde_json::json!({ "speed": 0.5, "item": "cup" });
        let bytes = codec.encode(&value).expect("encodes");
        assert_eq!(codec.decode(&bytes).expect("decodes"), value);
    }
}
"##;

#[test]
fn a_rust_node_that_asks_for_the_codec_api_lays_out_a_contract_at_run_time() {
    let temp_dir = TempDir::new_in(config_test_support::test_tmp_root())
        .expect("failed to create temp directory");
    let node_dir = temp_dir.path();
    let peppy_home = TempDir::new_in(config_test_support::test_tmp_root())
        .expect("failed to create temp directory");

    fs::write(node_dir.join(NODE_CONFIG_FILE), NODE_CONFIG).expect("writes peppy.json5");
    fs::write(
        node_dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
             [dependencies]\nserde_json = \"1\"\n",
            helpers::test_wrapper_crate_name(node_dir)
        ),
    )
    .expect("writes Cargo.toml");
    fs::create_dir_all(node_dir.join("src")).expect("creates src");
    fs::write(node_dir.join("src").join("main.rs"), NODE_MAIN).expect("writes main.rs");

    helpers::generate_test_peppygen_lib(PeppygenLanguage::Rust, node_dir, Vec::new())
        .expect("the library generates");

    let manifest_path = node_dir.join("Cargo.toml");
    let mut manifest: DocumentMut = fs::read_to_string(&manifest_path)
        .expect("reads Cargo.toml")
        .parse()
        .expect("Cargo.toml parses");
    assert_eq!(
        manifest["dependencies"]["peppy-codec"]["path"].as_str(),
        Some(daemon_config::consts::PEPPY_CODEC_OUTPUT_PATH)
    );
    assert!(
        node_dir
            .join(daemon_config::consts::PEPPY_CODEC_OUTPUT_PATH)
            .join("Cargo.toml")
            .is_file()
    );
    // The shared test target dir holds the `peppygen` of every test: the
    // node takes the one of its own under a name of its own.
    for table in ["dependencies", "dev-dependencies"] {
        manifest[table]["peppygen"]["package"] = value(helpers::peppygen_crate_name(node_dir));
    }
    fs::write(&manifest_path, manifest.to_string()).expect("writes Cargo.toml");

    helpers::run_cargo_test_with(node_dir, |command| {
        command
            .env_remove(peppy_codec::compiler::CAPNP_BINARY_PATH_ENV)
            .env(config::consts::PEPPY_HOME_ENV, peppy_home.path());
    });

    let extracted: Vec<String> = fs::read_dir(peppy_home.path().join("bin"))
        .expect("the node extracts its compiler into the bin directory of PEPPY_HOME")
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
