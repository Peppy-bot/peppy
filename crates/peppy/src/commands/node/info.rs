use config::AnyType;
use core_node::encoding::{NodeInfoRequest, NodeInfoResponse};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use super::source::parse_node_source;
use crate::commands::CALLER_INSTANCE_ID;
use crate::context::AppContext;
use crate::error::{Error, Result};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub fn node_info(ctx: &Arc<AppContext>, source: String, git_ref: Option<String>) -> Result<()> {
    crate::commands::block_on(node_info_async(ctx, source, git_ref))
}

async fn node_info_async(
    ctx: &Arc<AppContext>,
    source: String,
    git_ref: Option<String>,
) -> Result<()> {
    let node_source = parse_node_source(&source, git_ref)?;
    let conn = ctx.connect_to_daemon().await?;

    let request = NodeInfoRequest::new(node_source);

    let response = request
        .poll(
            conn.messenger,
            &conn.core_node_name,
            CALLER_INSTANCE_ID,
            &conn.core_node_name,
            REQUEST_TIMEOUT,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to get node info: {}", e)))?;

    let mut out = String::new();
    format_node_info(&mut out, &response);
    print!("{}", out);
    Ok(())
}

fn format_node_info(out: &mut String, response: &NodeInfoResponse) {
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

    // Variant info
    if let Some(ref variant_name) = response.variant_name {
        let _ = writeln!(out, "Variant:   {}", variant_name);
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

    // Node stack status
    let _ = writeln!(out);
    let _ = writeln!(out, "Node Stack Status");
    let _ = writeln!(out, "{}", "-".repeat(50));
    if response.is_in_node_stack {
        let _ = writeln!(out, "Status:    In node stack");
        if let Some(stage) = response.stage.as_deref() {
            let _ = writeln!(out, "Stage:     {}", stage);
        }
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
            let _ = writeln!(out, "{}:{}", manifest.name.as_str(), manifest.tag);
            if let Some(add_log_path) = response.add_log_path.as_ref() {
                let _ = writeln!(out, "  Add log: {}", add_log_path.display());
            }
            if has_run_logs {
                let _ = writeln!(out, "  Run logs:");
                // `run_log_paths` is aligned 1:1 with `instances` (same
                // order, same length) — see the info handler.
                for (instance, log_path) in response
                    .instances
                    .iter()
                    .zip(response.run_log_paths.iter())
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
    } else {
        let _ = writeln!(out, "Status:    Not in node stack");
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

    // Issues
    if !response.issues.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "Issues");
        let _ = writeln!(out, "{}", "-".repeat(50));
        for issue in &response.issues {
            let _ = writeln!(out, "  - {}", issue);
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
        for (key, value) in &config.execution.parameters {
            let _ = writeln!(out, "  {}: {}", key, format_any_type(value));
        }
    }

    let _ = writeln!(out);
}

fn format_any_type(value: &AnyType) -> String {
    match value {
        AnyType::Null => "null".to_string(),
        AnyType::Bool(b) => b.to_string(),
        AnyType::String(s) => format!("\"{}\"", s),
        AnyType::Int(i) => i.to_string(),
        AnyType::UInt(u) => u.to_string(),
        AnyType::Float(f) => f.to_string(),
        AnyType::Array(arr) => {
            let items: Vec<String> = arr.iter().map(format_any_type).collect();
            format!("[{}]", items.join(", "))
        }
        AnyType::Object(obj) => {
            let items: Vec<String> = obj
                .iter()
                .map(|(k, v)| format!("{}: {}", k, format_any_type(v)))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::node::NodeConfigParser;
    use core_node::encoding::NodeInstanceInfo;
    use std::path::PathBuf;

    fn sample_response() -> NodeInfoResponse {
        let config_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "sensor_node",
                tag: "0.1.0",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("peppy.json5");
        std::fs::write(&path, config_json5).expect("write config");
        let config = NodeConfigParser::from_path(&path)
            .expect("parse config")
            .into_resolved()
            .expect("resolve config");
        NodeInfoResponse {
            config,
            is_in_node_stack: true,
            instances_names: vec!["inst-abc".to_string()],
            config_integrity: "0".repeat(64),
            variant_name: None,
            issues: Vec::new(),
            stage: Some("Ready".to_string()),
            instances: vec![
                NodeInstanceInfo {
                    instance_id: "inst-abc".to_string(),
                    state: "running".to_string(),
                },
                NodeInstanceInfo {
                    instance_id: "inst-def".to_string(),
                    state: "starting".to_string(),
                },
            ],
            add_log_path: Some(PathBuf::from("/tmp/peppy/logs/add/sensor_node.log")),
            run_log_paths: vec![
                PathBuf::from("/tmp/peppy/logs/start/inst-abc.log"),
                PathBuf::from("/tmp/peppy/logs/start/inst-def.log"),
            ],
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
            out.contains("    - inst-abc: /tmp/peppy/logs/start/inst-abc.log"),
            "inst-abc run log line missing:\n{out}"
        );
        assert!(
            out.contains("    - inst-def: /tmp/peppy/logs/start/inst-def.log"),
            "inst-def run log line missing:\n{out}"
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
    fn print_node_info_skips_stage_when_not_in_stack() {
        let mut response = sample_response();
        response.is_in_node_stack = false;
        response.stage = None;
        response.instances.clear();
        response.instances_names.clear();
        response.add_log_path = None;
        response.run_log_paths.clear();

        let mut out = String::new();
        format_node_info(&mut out, &response);

        assert!(out.contains("Status:    Not in node stack"));
        assert!(!out.contains("Stage:"));
        assert!(!out.contains("\nLogs\n"));
    }
}
