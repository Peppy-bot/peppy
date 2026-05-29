use config::node::{NodeConfig, NodeConfigParser};

/// Returns a minimal core/root node configuration for tests.
/// The core node is the required root of every NodeStack.
pub fn core_node_config() -> NodeConfig {
    NodeConfigParser::from_content(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "core",
                tag: "v1",
            },
            execution: {
                language: "rust",
                run_cmd: ["core"]
            }
        }"#,
    )
    .expect("parse core node config")
}
