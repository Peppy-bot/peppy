use config::{DefaultValue, ParameterSpec, type_token_name};
use core_node_api::encoding::{NodeInfo, NodeInfoRequest, NodeInfoResponse};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use crate::commands::CALLER_INSTANCE_ID;
use crate::context::AppContext;
use crate::error::{Error, Result};

use peppylib::core_node::transport::poll_node_info;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub fn node_info(
    ctx: &Arc<AppContext>,
    node_name: String,
    node_tag: String,
    node_variant: String,
) -> Result<()> {
    crate::commands::block_on(node_info_async(ctx, node_name, node_tag, node_variant))
}

async fn node_info_async(
    ctx: &Arc<AppContext>,
    node_name: String,
    node_tag: String,
    node_variant: String,
) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    let response = poll_node_info(
        &NodeInfoRequest::new(node_name.clone(), node_tag.clone(), node_variant.clone()),
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        &conn.core_node_name,
        REQUEST_TIMEOUT,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("Failed to get node info: {}", e)))?;

    // "Not in stack" is now a first-class successful outcome of the lookup
    // rather than a daemon-side error. Surface it to the user as a clean
    // failure message without the generic "Failed to get node info:" prefix.
    let info = match response {
        NodeInfoResponse::NotInStack => {
            return Err(Error::ExecutionFailed(format!(
                "Node '{}' is not in the node stack",
                config::node::render_node_id(&node_name, &node_tag, &node_variant)
            )));
        }
        NodeInfoResponse::Found(info) => info,
    };

    // `info.run_log_paths` is aligned 1:1 with `info.instances` (same
    // order, same length) — see `NodeInfo` in
    // crates/core-node-internal/src/encoding/node/info.rs. `format_node_info`
    // consumes them via `.zip()`, which would silently truncate on drift.
    // Fail loud here instead so encoding/version mismatches surface clearly.
    if info.instances.len() != info.run_log_paths.len() {
        return Err(Error::ExecutionFailed(format!(
            "daemon returned mismatched node_info lengths: {} instances vs {} run log paths — this is an internal encoding bug",
            info.instances.len(),
            info.run_log_paths.len(),
        )));
    }

    let mut out = String::new();
    format_node_info(&mut out, &info);
    print!("{}", out);
    Ok(())
}

fn format_node_info(out: &mut String, response: &NodeInfo) {
    use std::fmt::Write as _;
    let config = &response.config;
    let manifest = &config.manifest;

    let _ = writeln!(out);
    let _ = writeln!(out, "Node Information");
    let _ = writeln!(out, "{}", "=".repeat(50));
    let _ = writeln!(out);

    // Basic info
    let _ = writeln!(out, "Name:      {}", manifest.name.as_str());
    let _ = writeln!(out, "Tag:       {}", manifest.tag);
    let _ = writeln!(out, "Language:  {:?}", config.execution.language);

    // Labels
    if let Some(labels) = &manifest.labels
        && !labels.is_empty()
    {
        let _ = writeln!(out, "Labels:    {}", labels.join(", "));
    }

    // Available variants (from manifest)
    if let Some(variants) = &manifest.variants
        && !variants.is_empty()
    {
        let names: Vec<&str> = variants.iter().map(|v| v.name.as_str()).collect();
        let _ = writeln!(out, "Variants:  {}", names.join(", "));
    }

    // Commands
    if let Some(build_cmd) = &config.execution.build_cmd {
        let _ = writeln!(out, "Build cmd:   {}", build_cmd.join(" "));
    }
    if let Some(run_cmd) = &config.execution.run_cmd {
        let _ = writeln!(out, "Run cmd: {}", run_cmd.join(" "));
    }
    if let Some(container) = &config.execution.container {
        let _ = writeln!(out, "Container: {}", container.def_file);
    }

    // Node stack status — the daemon only answers node_info requests for
    // nodes that are in the stack, so we always have a stage + instance list.
    let _ = writeln!(out);
    let _ = writeln!(out, "Node Stack Status");
    let _ = writeln!(out, "{}", "-".repeat(50));
    let _ = writeln!(out, "Stage:     {}", response.stage);
    // Always show the selected variant. The wire field is always
    // populated; a node added without an explicit variant carries the
    // literal `"default"` string (either auto-resolved from the root
    // manifest's `variants[]` or used because no variants were defined).
    let _ = writeln!(out, "Variant:   {}", response.variant);
    if response.instances.is_empty() {
        let _ = writeln!(out, "Instances: None tracked");
    } else {
        let _ = writeln!(out, "Instances: {} tracked", response.instances.len());
        for instance in &response.instances {
            let _ = writeln!(
                out,
                "           - {}  [{}]",
                instance.instance_id, instance.state
            );
        }
    }

    // Logs grouped under <name>:<tag>. Only printed when at least one
    // log path is available — if neither the add log nor any run log
    // is set, the section is suppressed entirely.
    let has_add_log = response.add_log_path.is_some();
    let has_run_logs = !response.run_log_paths.is_empty();
    if has_add_log || has_run_logs {
        let _ = writeln!(out);
        let _ = writeln!(out, "Logs");
        let _ = writeln!(out, "{}", "-".repeat(50));
        let _ = writeln!(
            out,
            "{}",
            config::node::render_node_id(manifest.name.as_str(), &manifest.tag, &response.variant)
        );
        if let Some(add_log_path) = response.add_log_path.as_ref() {
            let _ = writeln!(out, "  Add log: {}", add_log_path.display());
        }
        if has_run_logs {
            let _ = writeln!(out, "  Run logs:");
            // `run_log_paths` is aligned 1:1 with `instances` (same
            // order, same length) — see the info handler.
            for (instance, log_path) in response.instances.iter().zip(response.run_log_paths.iter())
            {
                let _ = writeln!(
                    out,
                    "    - {}: {}",
                    instance.instance_id,
                    log_path.display()
                );
            }
        }
    }

    // Dependencies (extracted from consumes interfaces)
    {
        let mut dependencies: BTreeSet<&str> = BTreeSet::new();

        if let Some(topics) = &config.interfaces.topics
            && let Some(expected) = &topics.consumes
        {
            for topic in expected {
                if let config::node::ConsumedTopic::Linked(linked) = topic {
                    dependencies.insert(&linked.local_node_id);
                }
            }
        }
        if let Some(services) = &config.interfaces.services
            && let Some(consumed) = &services.consumes
        {
            for service in consumed {
                dependencies.insert(&service.local_node_id);
            }
        }
        if let Some(actions) = &config.interfaces.actions
            && let Some(consumed) = &actions.consumes
        {
            for action in consumed {
                if !action.local_node_id.is_empty() {
                    dependencies.insert(&action.local_node_id);
                }
            }
        }

        if !dependencies.is_empty() {
            let _ = writeln!(out);
            let _ = writeln!(out, "Dependencies");
            let _ = writeln!(out, "{}", "-".repeat(50));
            for dep in &dependencies {
                let _ = writeln!(out, "  - {}", dep);
            }
        }
    }

    // Exposed Interfaces
    {
        let has_content = config
            .interfaces
            .topics
            .as_ref()
            .and_then(|t| t.emits.as_ref())
            .is_some_and(|t| !t.is_empty())
            || config
                .interfaces
                .services
                .as_ref()
                .and_then(|s| s.exposes.as_ref())
                .is_some_and(|s| !s.is_empty())
            || config
                .interfaces
                .actions
                .as_ref()
                .and_then(|a| a.exposes.as_ref())
                .is_some_and(|a| !a.is_empty());

        if has_content {
            let _ = writeln!(out);
            let _ = writeln!(out, "Exposed Interfaces");
            let _ = writeln!(out, "{}", "-".repeat(50));

            if let Some(topics) = config
                .interfaces
                .topics
                .as_ref()
                .and_then(|t| t.emits.as_ref())
                && !topics.is_empty()
            {
                let _ = writeln!(out, "Emitted Topics:");
                for topic in topics {
                    let _ = writeln!(out, "  - {} (qos: {:?})", topic.name, topic.qos_profile);
                }
            }

            if let Some(services) = config
                .interfaces
                .services
                .as_ref()
                .and_then(|s| s.exposes.as_ref())
                && !services.is_empty()
            {
                let _ = writeln!(out, "Services:");
                for service in services {
                    let _ = writeln!(out, "  - {}", service.name);
                }
            }

            if let Some(actions) = config
                .interfaces
                .actions
                .as_ref()
                .and_then(|a| a.exposes.as_ref())
                && !actions.is_empty()
            {
                let _ = writeln!(out, "Actions:");
                for action in actions {
                    let _ = writeln!(out, "  - {}", action.name);
                }
            }
        }
    }

    // Consumed Interfaces
    {
        let has_content = config
            .interfaces
            .topics
            .as_ref()
            .and_then(|t| t.consumes.as_ref())
            .is_some_and(|t| !t.is_empty())
            || config
                .interfaces
                .services
                .as_ref()
                .and_then(|s| s.consumes.as_ref())
                .is_some_and(|s| !s.is_empty())
            || config
                .interfaces
                .actions
                .as_ref()
                .and_then(|a| a.consumes.as_ref())
                .is_some_and(|a| !a.is_empty());

        if has_content {
            let _ = writeln!(out);
            let _ = writeln!(out, "Consumed Interfaces");
            let _ = writeln!(out, "{}", "-".repeat(50));

            if let Some(topics) = config
                .interfaces
                .topics
                .as_ref()
                .and_then(|t| t.consumes.as_ref())
                && !topics.is_empty()
            {
                let _ = writeln!(out, "Consumed Topics:");
                for topic in topics {
                    match topic {
                        config::node::ConsumedTopic::Linked(linked) => {
                            let _ = writeln!(
                                out,
                                "  - {} (from node: {})",
                                linked.name, linked.local_node_id
                            );
                        }
                        config::node::ConsumedTopic::External(external) => {
                            let _ = writeln!(out, "  - {} (external)", external.name);
                        }
                    }
                }
            }

            if let Some(services) = config
                .interfaces
                .services
                .as_ref()
                .and_then(|s| s.consumes.as_ref())
                && !services.is_empty()
            {
                let _ = writeln!(out, "Services:");
                for service in services {
                    let _ = writeln!(
                        out,
                        "  - {} (from node: {})",
                        service.name, service.local_node_id
                    );
                }
            }

            if let Some(actions) = config
                .interfaces
                .actions
                .as_ref()
                .and_then(|a| a.consumes.as_ref())
                && !actions.is_empty()
            {
                let _ = writeln!(out, "Actions:");
                for action in actions {
                    let _ = writeln!(
                        out,
                        "  - {} (from node: {})",
                        action.name, action.local_node_id
                    );
                }
            }
        }
    }

    // Integrity
    let _ = writeln!(out);
    let _ = writeln!(out, "Integrity");
    let _ = writeln!(out, "{}", "-".repeat(50));
    let _ = writeln!(out, "Config SHA256: {}", response.config_integrity);

    // Parameters
    if !config.execution.parameters.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "Parameters");
        let _ = writeln!(out, "{}", "-".repeat(50));
        for (key, spec) in &config.execution.parameters {
            write_parameter_spec(out, key, spec, 1);
        }
    }

    let _ = writeln!(out);
}

/// Render a parameter declaration: a primitive shows its type and optional
/// `$default`; a group shows its name and recurses on its leaves; an array
/// shows its item type and optional fixed length.
fn write_parameter_spec(out: &mut String, key: &str, spec: &ParameterSpec, indent: usize) {
    use std::fmt::Write as _;
    let pad = "  ".repeat(indent);
    match spec {
        ParameterSpec::Primitive { kind, default } => {
            let type_name = type_token_name(kind);
            match default {
                Some(d) => {
                    let _ = writeln!(
                        out,
                        "{}{}: {} = {}",
                        pad,
                        key,
                        type_name,
                        format_default_value_inline(d)
                    );
                }
                None => {
                    let _ = writeln!(out, "{}{}: {}", pad, key, type_name);
                }
            }
        }
        ParameterSpec::Array { items, length } => {
            let item_type = describe_parameter_spec_inline(items);
            match length {
                Some(l) => {
                    let _ = writeln!(out, "{}{}: array[{}; {}]", pad, key, item_type, l);
                }
                None => {
                    let _ = writeln!(out, "{}{}: array[{}]", pad, key, item_type);
                }
            }
        }
        ParameterSpec::Group(map) => {
            let _ = writeln!(out, "{}{}:", pad, key);
            for (child_key, child_spec) in map {
                write_parameter_spec(out, child_key, child_spec, indent + 1);
            }
        }
    }
}

/// One-line description used for array `$items`: just the type, no key.
fn describe_parameter_spec_inline(spec: &ParameterSpec) -> String {
    match spec {
        ParameterSpec::Primitive { kind, .. } => type_token_name(kind).to_string(),
        ParameterSpec::Array { items, length } => {
            let inner = describe_parameter_spec_inline(items);
            match length {
                Some(l) => format!("array[{inner}; {l}]"),
                None => format!("array[{inner}]"),
            }
        }
        ParameterSpec::Group(_) => "object".to_string(),
    }
}

fn format_default_value_inline(d: &DefaultValue) -> String {
    match d {
        DefaultValue::Bool(b) => b.to_string(),
        // Use serde_json to escape quotes, backslashes, control chars, etc.
        // so the rendered output is a valid quoted string for any input.
        DefaultValue::String(s) => serde_json::to_string(s).unwrap_or_else(|_| {
            format!(
                "\"{}\"",
                s.replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', "\\n")
            )
        }),
        DefaultValue::Int(i) => i.to_string(),
        DefaultValue::UInt(u) => u.to_string(),
        DefaultValue::Float(f) => f.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::node::NodeConfigParser;
    use core_node_api::encoding::NodeInstanceInfo;
    use core_node_api::{InstanceState, NodeStage};
    use std::path::PathBuf;

    fn sample_response() -> NodeInfo {
        sample_response_with_execution(
            r#"{
                language: "rust",
                run_cmd: ["sleep", "10"]
            }"#,
        )
    }

    fn sample_response_with_execution(execution_json5: &str) -> NodeInfo {
        let config_json5 = format!(
            r#"{{
                peppy_schema: "node_v1",
                manifest: {{
                    name: "sensor_node",
                    tag: "0.1.0",
                }},
                execution: {execution_json5}
            }}"#
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("peppy.json5");
        std::fs::write(&path, config_json5).expect("write config");
        let config = NodeConfigParser::from_path(&path)
            .expect("parse config")
            .into_resolved()
            .expect("resolve config");
        NodeInfo {
            config,
            config_integrity: "0".repeat(64),
            stage: NodeStage::Ready,
            instances: vec![
                NodeInstanceInfo {
                    instance_id: "inst-abc".to_string(),
                    state: InstanceState::Running,
                },
                NodeInstanceInfo {
                    instance_id: "inst-def".to_string(),
                    state: InstanceState::Starting,
                },
            ],
            add_log_path: Some(PathBuf::from("/tmp/peppy/logs/add/sensor_node.log")),
            run_log_paths: vec![
                PathBuf::from("/tmp/peppy/logs/run/inst-abc.log"),
                PathBuf::from("/tmp/peppy/logs/run/inst-def.log"),
            ],
            variant: config::node::DEFAULT_VARIANT_NAME.to_owned(),
        }
    }

    #[test]
    fn print_node_info_renders_stage_instances_and_logs() {
        let response = sample_response();
        let mut out = String::new();
        format_node_info(&mut out, &response);

        assert!(
            out.contains("Stage:     Ready"),
            "stage line missing:\n{out}"
        );
        assert!(
            out.contains("Instances: 2 tracked"),
            "instance count missing:\n{out}"
        );
        assert!(
            out.contains("- inst-abc  [running]"),
            "running instance line missing:\n{out}"
        );
        assert!(
            out.contains("- inst-def  [starting]"),
            "starting instance line missing:\n{out}"
        );

        // Logs section grouped under <name>:<tag>
        assert!(out.contains("\nLogs\n"), "Logs heading missing:\n{out}");
        assert!(
            out.contains("sensor_node:0.1.0"),
            "node:tag heading missing:\n{out}"
        );
        assert!(
            out.contains("  Add log: /tmp/peppy/logs/add/sensor_node.log"),
            "add log line missing:\n{out}"
        );
        assert!(
            out.contains("  Run logs:"),
            "run logs sub-heading missing:\n{out}"
        );
        assert!(
            out.contains("    - inst-abc: /tmp/peppy/logs/run/inst-abc.log"),
            "inst-abc run log line missing:\n{out}"
        );
        assert!(
            out.contains("    - inst-def: /tmp/peppy/logs/run/inst-def.log"),
            "inst-def run log line missing:\n{out}"
        );
    }

    #[test]
    fn print_node_info_renders_variant_line_as_default_when_default() {
        let response = sample_response();
        assert_eq!(
            response.variant,
            config::node::DEFAULT_VARIANT_NAME,
            "precondition"
        );

        let mut out = String::new();
        format_node_info(&mut out, &response);

        assert!(
            out.contains("Variant:   default"),
            "Variant line should render 'default' when variant is the implicit default:\n{out}"
        );
    }

    #[test]
    fn print_node_info_renders_variant_line_when_explicit() {
        let mut response = sample_response();
        response.variant = "macos".to_string();

        let mut out = String::new();
        format_node_info(&mut out, &response);

        assert!(
            out.contains("Variant:   macos"),
            "Variant line missing when variant is set:\n{out}"
        );
        // Variant must appear inside the Node Stack Status section, between
        // the Stage line and the Instances line.
        let stage_idx = out.find("Stage:").expect("stage line missing");
        let variant_idx = out.find("Variant:").expect("variant line missing");
        let instances_idx = out.find("Instances:").expect("instances line missing");
        assert!(
            stage_idx < variant_idx && variant_idx < instances_idx,
            "Variant line should sit between Stage and Instances:\n{out}"
        );
    }

    #[test]
    fn print_node_info_omits_logs_when_none_present() {
        let mut response = sample_response();
        response.add_log_path = None;
        response.run_log_paths.clear();

        let mut out = String::new();
        format_node_info(&mut out, &response);

        assert!(
            !out.contains("\nLogs\n"),
            "Logs section should be suppressed when no paths are present:\n{out}"
        );
    }

    #[test]
    fn print_node_info_renders_nested_parameters_as_indented_tree() {
        let response = sample_response_with_execution(
            r#"{
                language: "rust",
                run_cmd: ["sleep", "10"],
                parameters: {
                    device_path: "string",
                    video: {
                        camera_encoding: "string",
                        frame_rate: "u16",
                        resolution: {
                            height: "u32",
                            width: "u32",
                        },
                        topic_encoding: "string",
                    },
                }
            }"#,
        );

        let mut out = String::new();
        format_node_info(&mut out, &response);

        // Primitive parameters render as `name: type`.
        assert!(
            out.contains("  device_path: string"),
            "primitive parameter missing:\n{out}"
        );

        // Parent group renders as a bare key with its children on subsequent lines.
        assert!(
            out.contains("  video:\n    camera_encoding: string"),
            "nested parameter not rendered as indented tree:\n{out}"
        );
        assert!(
            out.contains("    frame_rate: u16"),
            "second-level child missing:\n{out}"
        );
        // Deeply nested (resolution) indented one more level.
        assert!(
            out.contains("    resolution:\n      height: u32"),
            "third-level child missing:\n{out}"
        );
        assert!(
            out.contains("      width: u32"),
            "third-level child missing:\n{out}"
        );
    }

    #[test]
    fn print_node_info_renders_primitive_with_default() {
        let response = sample_response_with_execution(
            r#"{
                language: "rust",
                run_cmd: ["sleep", "10"],
                parameters: {
                    device_path: { $type: "string", $default: "/dev/video0" },
                    frame_rate: { $type: "u16", $default: 30 },
                }
            }"#,
        );

        let mut out = String::new();
        format_node_info(&mut out, &response);

        assert!(
            out.contains("  device_path: string = \"/dev/video0\""),
            "primitive with string default missing:\n{out}"
        );
        assert!(
            out.contains("  frame_rate: u16 = 30"),
            "primitive with int default missing:\n{out}"
        );
    }

    #[test]
    fn format_default_value_inline_escapes_special_characters() {
        let cases = [
            (DefaultValue::String("plain".into()), r#""plain""#),
            (
                DefaultValue::String("with\"quote".into()),
                r#""with\"quote""#,
            ),
            (
                DefaultValue::String("back\\slash".into()),
                r#""back\\slash""#,
            ),
            (
                DefaultValue::String("line\nbreak".into()),
                r#""line\nbreak""#,
            ),
        ];
        for (input, expected) in cases {
            let actual = format_default_value_inline(&input);
            assert_eq!(actual, expected, "input was {input:?}");
        }
    }

    #[test]
    fn print_node_info_renders_array_parameter_with_item_type() {
        let response = sample_response_with_execution(
            r#"{
                language: "rust",
                run_cmd: ["sleep", "10"],
                parameters: {
                    tags: { $type: "array", $items: "string" },
                }
            }"#,
        );

        let mut out = String::new();
        format_node_info(&mut out, &response);

        assert!(
            out.contains("  tags: array[string]"),
            "array parameter should render with item type:\n{out}"
        );
    }
}
