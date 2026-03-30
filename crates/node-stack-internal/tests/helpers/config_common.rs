use config::node::{NodeConfig, NodeConfigParser};

/// Returns a minimal core/root node configuration for tests.
/// The core node is the required root of every NodeStack.
pub fn core_node_config() -> NodeConfig {
    NodeConfigParser::from_content(
        r#"{
            schema_version: 1,
            manifest: {
                name: "core",
                tag: "1.0.0",
            },
            execution: {
                language: "rust",
                start_cmd: ["core"]
            }
        }"#,
    )
    .expect("parse core node config")
    .into_resolved()
    .expect("test config has execution")
}
