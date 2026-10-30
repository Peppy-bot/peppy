use config::node::{Interfaces, ResolvedRestart, TopicRetention};
use config::{DefaultValue, ParameterSpec, type_token_name};
use core_node_api::encoding::NodeInstanceInfo;
use core_node_api::encoding::{NodeInfo, NodeInfoRequest, NodeInfoResponse};
use core_node_api::{InstanceState, RestartStatus};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use crate::commands::CALLER_INSTANCE_ID;
use crate::context::AppContext;
use crate::error::{Error, Result};

use peppylib::core_node::transport::poll;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub fn node_info(ctx: &Arc<AppContext>, node_name: String, node_tag: String) -> Result<()> {
    crate::commands::block_on(node_info_async(ctx, node_name, node_tag))
}

async fn node_info_async(ctx: &Arc<AppContext>, node_name: String, node_tag: String) -> Result<()> {
    print!("{}", node_info_rendered(ctx, &node_name, &node_tag).await?);
    Ok(())
}

/// What `peppy node info` prints for `node_name:node_tag`.
pub async fn node_info_rendered(
    ctx: &Arc<AppContext>,
    node_name: &str,
    node_tag: &str,
) -> Result<String> {
    let conn = ctx.connect_to_daemon().await?;

    let response = poll(
        &NodeInfoRequest::new(node_name, node_tag),
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        &conn.target_core_node,
        REQUEST_TIMEOUT,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("Failed to get node info: {}", e)))?;

    // "Not in stack" is a successful outcome of the lookup, not a daemon-side
    // error. Surface it to the user as a clean failure message without the
    // generic "Failed to get node info:" prefix.
    let info = match response {
        NodeInfoResponse::NotInStack => {
            return Err(Error::ExecutionFailed(format!(
                "Node '{}:{}' is not in the node stack",
                node_name, node_tag
            )));
        }
        NodeInfoResponse::Found(info) => info,
    };

    // `info.run_log_paths` is aligned 1:1 with `info.instances` (same
    // order, same length); see `NodeInfo` in
    // crates/core-node-internal/src/encoding/node/info.rs. `format_node_info`
    // consumes them via `.zip()`, which would silently truncate on drift.
    // Fail loud here instead so encoding/version mismatches surface clearly.
    if info.instances.len() != info.run_log_paths.len() {
        return Err(Error::ExecutionFailed(format!(
            "daemon returned mismatched node_info lengths: {} instances vs {} run log paths; this is an internal encoding bug",
            info.instances.len(),
            info.run_log_paths.len(),
        )));
    }

    let mut out = String::new();
    format_node_info(&mut out, &info, crate::terminal::stdout_width());
    Ok(out)
}

/// Renders the node's info, its description wrapped to `max_width` columns
/// (`None` keeps each paragraph on one line); a restarting instance's wait
/// counts down from the daemon's clock when it answered.
fn format_node_info(out: &mut String, response: &NodeInfo, max_width: Option<usize>) {
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

    write_section_heading(out, "Description");
    out.push_str(&crate::commands::prose::paragraphs(
        &manifest.description,
        "",
        max_width,
    ));

    // Node stack status: the daemon only answers node_info requests for
    // nodes that are in the stack, so we always have a stage + instance list.
    write_section_heading(out, "Node Stack Status");
    let _ = writeln!(out, "Stage:     {}", response.stage);
    if response.instances.is_empty() {
        let _ = writeln!(out, "Instances: None tracked");
    } else {
        let _ = writeln!(out, "Instances: {} tracked", response.instances.len());
        for instance in &response.instances {
            // Health is the daemon's last `node_health` probe result, carried
            // in `NodeInstanceInfo::healthy`; `healthy` until a probe is seen
            // to fail. Shown alongside state so a running-but-failing instance
            // stands out without a separate `stack list` round-trip. An instance
            // without a process (restarting, finished or failed) has no live
            // health, so it renders `-`.
            let health = crate::commands::instance_health_label(instance.state, instance.healthy);
            let _ = writeln!(
                out,
                "           - {}  [{}]  {}  {}",
                instance.instance_id,
                instance.state,
                health,
                restart_summary(instance, response.reported_at_ms)
            );
            // The endpoints the instance serves, one line per URL under the
            // label and kind its manifest declares.
            for endpoint in &instance.endpoints {
                for url in &endpoint.urls {
                    let _ = writeln!(
                        out,
                        "               {} ({})  {url}",
                        endpoint.label, endpoint.kind
                    );
                }
            }
        }
    }

    // Logs grouped under <name>:<tag>. Only printed when at least one
    // log path is available; if neither the add log nor any run log
    // is set, the section is suppressed entirely.
    let has_add_log = response.add_log_path.is_some();
    let has_run_logs = !response.run_log_paths.is_empty();
    if has_add_log || has_run_logs {
        write_section_heading(out, "Logs");
        let _ = writeln!(out, "{}:{}", manifest.name.as_str(), manifest.tag);
        if let Some(add_log_path) = response.add_log_path.as_ref() {
            let _ = writeln!(out, "  Add log: {}", add_log_path.display());
        }
        if has_run_logs {
            let _ = writeln!(out, "  Run logs:");
            // `run_log_paths` is aligned 1:1 with `instances` (same
            // order, same length); see the info handler.
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

    write_dependencies(out, &config.interfaces);
    write_exposed_interfaces(out, &config.interfaces);
    write_consumed_interfaces(out, &config.interfaces);

    write_section_heading(out, "Integrity");
    let _ = writeln!(out, "Config SHA256: {}", response.config_integrity);

    // Parameters
    if !config.execution.parameters.is_empty() {
        write_section_heading(out, "Parameters");
        for (key, spec) in &config.execution.parameters {
            write_parameter_spec(out, key, spec, 1);
        }
    }

    let _ = writeln!(out);
}

/// Writes the heading of one section: a blank line, the title and a dashed
/// rule.
fn write_section_heading(out: &mut String, title: &str) {
    let _ = writeln!(out);
    let _ = writeln!(out, "{title}");
    let _ = writeln!(out, "{}", "-".repeat(50));
}

/// The list when the config declares it with at least one entry.
fn filter_non_empty<T>(list: Option<&[T]>) -> Option<&[T]> {
    list.filter(|entries| !entries.is_empty())
}

/// Writes the slots the node consumes through, once each and sorted; the
/// section is left out when the node consumes nothing.
fn write_dependencies(out: &mut String, interfaces: &Interfaces) {
    let mut dependencies: BTreeSet<&str> = BTreeSet::new();

    if let Some(topics) = &interfaces.topics
        && let Some(expected) = &topics.consumes
    {
        for topic in expected {
            dependencies.insert(&topic.link_id);
        }
    }
    if let Some(services) = &interfaces.services
        && let Some(consumed) = &services.consumes
    {
        for service in consumed {
            dependencies.insert(&service.link_id);
        }
    }
    if let Some(actions) = &interfaces.actions
        && let Some(consumed) = &actions.consumes
    {
        for action in consumed {
            if !action.link_id.is_empty() {
                dependencies.insert(&action.link_id);
            }
        }
    }

    if dependencies.is_empty() {
        return;
    }
    write_section_heading(out, "Dependencies");
    for dep in &dependencies {
        let _ = writeln!(out, "  - {}", dep);
    }
}

/// Writes the topics the node emits and the services and actions it exposes;
/// the section is left out when every list is empty.
fn write_exposed_interfaces(out: &mut String, interfaces: &Interfaces) {
    let emitted_topics =
        filter_non_empty(interfaces.topics.as_ref().and_then(|t| t.emits.as_deref()));
    let services = filter_non_empty(
        interfaces
            .services
            .as_ref()
            .and_then(|s| s.exposes.as_deref()),
    );
    let actions = filter_non_empty(
        interfaces
            .actions
            .as_ref()
            .and_then(|a| a.exposes.as_deref()),
    );
    if emitted_topics.is_none() && services.is_none() && actions.is_none() {
        return;
    }

    write_section_heading(out, "Exposed Interfaces");
    if let Some(topics) = emitted_topics {
        let _ = writeln!(out, "Emitted Topics:");
        for topic in topics {
            match topic.as_native() {
                Some(native) => {
                    let _ = writeln!(
                        out,
                        "  - {} (qos: {:?}{})",
                        native.name,
                        native.qos_profile,
                        retention_note(native.retention)
                    );
                }
                None => {
                    let _ = writeln!(out, "  - {}{}", topic.name(), via_slot(topic.link_id()));
                }
            }
        }
    }
    if let Some(services) = services {
        let _ = writeln!(out, "Services:");
        for service in services {
            let _ = writeln!(out, "  - {}{}", service.name(), via_slot(service.link_id()));
        }
    }
    if let Some(actions) = actions {
        let _ = writeln!(out, "Actions:");
        for action in actions {
            let _ = writeln!(out, "  - {}{}", action.name(), via_slot(action.link_id()));
        }
    }
}

/// Writes the topics, services and actions the node consumes, each with the
/// slot it consumes through; the section is left out when every list is empty.
fn write_consumed_interfaces(out: &mut String, interfaces: &Interfaces) {
    let topics = filter_non_empty(
        interfaces
            .topics
            .as_ref()
            .and_then(|t| t.consumes.as_deref()),
    );
    let services = filter_non_empty(
        interfaces
            .services
            .as_ref()
            .and_then(|s| s.consumes.as_deref()),
    );
    let actions = filter_non_empty(
        interfaces
            .actions
            .as_ref()
            .and_then(|a| a.consumes.as_deref()),
    );
    if topics.is_none() && services.is_none() && actions.is_none() {
        return;
    }

    write_section_heading(out, "Consumed Interfaces");
    if let Some(topics) = topics {
        let _ = writeln!(out, "Consumed Topics:");
        for topic in topics {
            let _ = writeln!(out, "  - {} (via slot: {})", topic.name, topic.link_id);
        }
    }
    if let Some(services) = services {
        let _ = writeln!(out, "Services:");
        for service in services {
            let _ = writeln!(out, "  - {} (via slot: {})", service.name, service.link_id);
        }
    }
    if let Some(actions) = actions {
        let _ = writeln!(out, "Actions:");
        for action in actions {
            let _ = writeln!(out, "  - {} (via slot: {})", action.name, action.link_id);
        }
    }
}

/// What a native emitted topic's line says of its retention: nothing for a
/// live-only topic.
pub(super) fn retention_note(retention: TopicRetention) -> String {
    match retention {
        TopicRetention::LiveOnly => String::new(),
        retaining => format!(", retention: {retaining}"),
    }
}

/// Suffix naming the `manifest.implements` slot a contract-backed produced
/// entry resolves through; empty for a native entry.
fn via_slot(link_id: Option<&str>) -> String {
    link_id
        .map(|link_id| format!(" (via implements slot: {link_id})"))
        .unwrap_or_default()
}

/// Render a parameter declaration: a primitive shows its type and optional
/// `$default`; a group shows its name and recurses on its leaves; an array
/// shows its item type and optional fixed length.
fn write_parameter_spec(out: &mut String, key: &str, spec: &ParameterSpec, indent: usize) {
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
        DefaultValue::String(s) => {
            serde_json::to_string(s).expect("a string always serializes to JSON")
        }
        DefaultValue::Int(i) => i.to_string(),
        DefaultValue::UInt(u) => u.to_string(),
        DefaultValue::Float(f) => f.to_string(),
    }
}

/// The restart policy of an instance with its origin and count, then, in one
/// order for every state: the attempt a restarting instance waits for or a
/// failed one was refused, the last exit, and the next start of a restarting
/// instance from `reported_at_ms`, the daemon's clock when it answered, in
/// whole seconds rounded up.
fn restart_summary(instance: &NodeInstanceInfo, reported_at_ms: u64) -> String {
    let status = &instance.restart_status;
    let resolved = ResolvedRestart {
        declaration: status.restart,
        origin: status.restart_origin,
    };
    let mut parts = vec![
        format!("restart {resolved}"),
        match status.restart_count {
            1 => "1 restart".to_owned(),
            count => format!("{count} restarts"),
        },
    ];
    if instance.state == InstanceState::Restarting {
        parts.push(format!("attempt {}", status.attempt));
    }
    match restarts_ended(status, instance.state) {
        Some(RestartsEnded::Capped(max_restarts)) => parts.push(format!(
            "attempt {} refused: max_restarts {max_restarts} reached, failed starts included",
            status.attempt
        )),
        Some(RestartsEnded::Refused) => parts.push(format!("attempt {} refused", status.attempt)),
        None => {}
    }
    if let Some(exit) = &status.last_exit {
        parts.push(format!("last exit: {exit}"));
    }
    if instance.state == InstanceState::Restarting
        && let Some(next_start_ms) = status.next_start_ms
    {
        let wait_ms = next_start_ms.saturating_sub(reported_at_ms);
        parts.push(if wait_ms == 0 {
            "next start due; it waits for the last exit's notices to other daemons, or for a stack \
             launch, build, join or removal or a `peppy node remove` in progress on this daemon"
                .to_owned()
        } else {
            format!("next start in {} s", wait_ms.div_ceil(1000))
        });
    }
    parts.join(", ")
}

/// How a cap or the daemon ended the restarts of a `failed` instance.
#[derive(Debug, PartialEq)]
enum RestartsEnded {
    /// The attempt past `max_restarts`, which it carries, was refused.
    Capped(std::num::NonZeroU32),
    /// The daemon refused to start the instance again.
    Refused,
}

/// How the restarts of an instance in `state` ended: a policy that restarts
/// an instance moves its exit to `restarting`, so a `failed` instance with an
/// attempt is one whose restart the cap or the daemon refused.
fn restarts_ended(status: &RestartStatus, state: InstanceState) -> Option<RestartsEnded> {
    if state != InstanceState::Failed || status.attempt == 0 {
        return None;
    }
    Some(
        status
            .restart
            .max_restarts()
            .filter(|max_restarts| status.attempt > max_restarts.get())
            .map_or(RestartsEnded::Refused, RestartsEnded::Capped),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use config::node::NodeConfigParser;
    use config::node::RestartDeclaration;
    use core_node_api::encoding::NodeInstanceInfo;
    use core_node_api::{InstanceState, NodeStage};
    use std::path::PathBuf;

    /// A failed instance's restarts ended at the cap once its attempt is past
    /// `max_restarts`, by the daemon's refusal below it, and not at all with
    /// no attempt or in another state.
    #[test]
    fn restarts_end_at_the_cap_or_by_a_refusal() {
        let status = |max_restarts: Option<u32>, attempt: u32| RestartStatus {
            restart: RestartDeclaration::new(
                config::node::RestartPolicy::OnFailure,
                max_restarts.and_then(std::num::NonZeroU32::new),
            )
            .unwrap(),
            attempt,
            ..RestartStatus::default()
        };
        let failed = InstanceState::Failed;
        assert_eq!(
            restarts_ended(&status(Some(2), 3), failed),
            Some(RestartsEnded::Capped(std::num::NonZeroU32::new(2).unwrap()))
        );
        assert_eq!(
            restarts_ended(&status(Some(2), 2), failed),
            Some(RestartsEnded::Refused)
        );
        assert_eq!(
            restarts_ended(&status(None, 4), failed),
            Some(RestartsEnded::Refused)
        );
        assert_eq!(restarts_ended(&status(Some(2), 0), failed), None);
        assert_eq!(
            restarts_ended(&status(Some(2), 3), InstanceState::Restarting),
            None
        );
        assert_eq!(
            restarts_ended(&status(None, 1), InstanceState::Finished),
            None
        );
    }

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
                peppy_schema: "node/v1",
                manifest: {{
                    name: "sensor_node",
                    tag: "v1",
                    description: [
                        "A sensor on the bench: it publishes what it reads, once a second.",
                        "It needs no device.",
                    ],
                }},
                interfaces: {{}},
                execution: {execution_json5}
            }}"#
        );
        let config = NodeConfigParser::from_content(&config_json5).expect("parse config");
        NodeInfo {
            config,
            config_integrity: "0".repeat(64),
            stage: NodeStage::Ready,
            instances: vec![
                NodeInstanceInfo {
                    clock: Default::default(),
                    instance_id: "inst-abc".to_string(),
                    state: InstanceState::Running,
                    healthy: true,
                    slot_bindings: std::collections::BTreeMap::new(),
                    pairing_slots: std::collections::BTreeMap::new(),
                    endpoints: vec![
                        core_node_api::InstanceEndpoint {
                            label: "panel".to_string(),
                            kind: config::node::EndpointKind::Page,
                            urls: vec![
                                "http://127.0.0.1:8765".to_string(),
                                "http://192.168.1.5:8765".to_string(),
                            ],
                        },
                        core_node_api::InstanceEndpoint {
                            label: "sensor_v1".to_string(),
                            kind: config::node::EndpointKind::Mcp,
                            urls: vec!["http://127.0.0.1:8900/sensor/v1/mcp".to_string()],
                        },
                    ],
                    restart_status: core_node_api::RestartStatus::default(),
                },
                NodeInstanceInfo {
                    clock: Default::default(),
                    instance_id: "inst-def".to_string(),
                    state: InstanceState::Starting,
                    healthy: false,
                    slot_bindings: std::collections::BTreeMap::new(),
                    pairing_slots: std::collections::BTreeMap::new(),
                    endpoints: Vec::new(),
                    restart_status: core_node_api::RestartStatus::default(),
                },
            ],
            add_log_path: Some(PathBuf::from("/tmp/peppy/logs/add/sensor_node.log")),
            run_log_paths: vec![
                PathBuf::from("/tmp/peppy/logs/run/inst-abc.log"),
                PathBuf::from("/tmp/peppy/logs/run/inst-def.log"),
            ],
            reported_at_ms: 1_700_000_000_000,
        }
    }

    fn format_with_config(config_json5: &str) -> String {
        let config = NodeConfigParser::from_content(config_json5).expect("parse config");
        let mut out = String::new();
        format_node_info(
            &mut out,
            &NodeInfo {
                config,
                ..sample_response()
            },
            None,
        );
        out
    }

    /// A native emitted topic's line states its QoS, and its retention when it
    /// retains.
    #[test]
    fn print_node_info_states_the_retention_of_a_retaining_topic() {
        let out = format_with_config(
            r#"{
                peppy_schema: "node/v1",
                manifest: { name: "arm", tag: "v1", description: ["A test node."] },
                interfaces: {
                    topics: { emits: [
                        { name: "joint_state", description: "A test topic.", qos_profile: "sensor_data", message_format: { position: "f64" } },
                        { name: "robot_mode", description: "A test topic.", qos_profile: "reliable", retention: { latest: 3 }, message_format: { mode: "string" } },
                    ] },
                },
                execution: { language: "rust", run_cmd: ["./bin"] },
            }"#,
        );

        assert!(out.contains("  - joint_state (qos: SensorData)\n"), "{out}");
        assert!(
            out.contains("  - robot_mode (qos: Reliable, retention: latest 3)\n"),
            "{out}"
        );
    }

    /// The full description of the node follows its basic facts, one
    /// paragraph at a time, wrapped to the width limit.
    #[test]
    fn print_node_info_prints_the_description_of_the_node() {
        let response = sample_response();
        let mut out = String::new();
        format_node_info(&mut out, &response, Some(40));

        assert!(
            out.contains(
                "\nDescription\n--------------------------------------------------\n\
                 A sensor on the bench: it publishes what\n\
                 it reads, once a second.\n\
                 \n\
                 It needs no device.\n"
            ),
            "{out}"
        );
    }

    /// Each endpoint of an instance is listed under its instance line as
    /// `label (kind)  url`, one line per URL; an instance serving nothing
    /// gets no such line.
    #[test]
    fn print_node_info_lists_endpoints_under_their_instance() {
        let response = sample_response();
        let mut out = String::new();
        format_node_info(&mut out, &response, None);

        let lines: Vec<&str> = out.lines().collect();
        let instance_line = lines
            .iter()
            .position(|line| line.contains("- inst-abc  [running]  healthy"))
            .expect("the running instance line");
        assert_eq!(
            &lines[instance_line + 1..instance_line + 4],
            [
                "               panel (page)  http://127.0.0.1:8765",
                "               panel (page)  http://192.168.1.5:8765",
                "               sensor_v1 (mcp)  http://127.0.0.1:8900/sensor/v1/mcp",
            ]
        );
        let starting_line = lines
            .iter()
            .position(|line| line.contains("- inst-def  [starting]  unhealthy"))
            .expect("the starting instance line");
        assert!(
            !lines[starting_line + 1].contains("http"),
            "an instance serving nothing lists no endpoint: {}",
            lines[starting_line + 1]
        );
    }

    /// Each instance line carries its policy with the origin and the
    /// count, then what its state adds: the last exit for a running or
    /// failed instance, the attempt and the next start while it waits, and
    /// for a failed one the attempt its cap or the daemon refused.
    #[test]
    fn print_node_info_renders_the_restart_summary() {
        let mut response = sample_response();
        let now = response.reported_at_ms;
        response.instances[0].restart_status.restart = RestartDeclaration::new(
            config::node::RestartPolicy::OnFailure,
            std::num::NonZeroU32::new(5),
        )
        .unwrap();
        response.instances[0].restart_status.restart_origin = config::node::RestartOrigin::Instance;
        response.instances[0].restart_status.restart_count = 2;
        response.instances[0].restart_status.last_exit =
            Some(core_node_api::ExitCause::signal(9, false));
        response.instances[1].state = InstanceState::Restarting;
        response.instances[1].restart_status.restart =
            RestartDeclaration::policy_only(config::node::RestartPolicy::Always);
        response.instances[1].restart_status.restart_origin =
            config::node::RestartOrigin::LauncherRoot;
        response.instances[1].restart_status.attempt = 3;
        response.instances[1].restart_status.last_exit = Some(core_node_api::ExitCause::Code(1));
        response.instances[1].restart_status.next_start_ms = Some(now + 60_000);
        let rendered = |response: &NodeInfo| {
            let mut out = String::new();
            format_node_info(&mut out, response, None);
            out
        };
        // A running instance names no attempt, whatever its backoff holds.
        response.instances[0].restart_status.attempt = 2;
        let out = rendered(&response);
        assert!(
            out.contains(
                "- inst-abc  [running]  healthy  restart on_failure, max_restarts 5 (instance \
                 entry), 2 restarts, last exit: signal 9 (SIGKILL)\n"
            ),
            "{out}"
        );
        assert!(
            out.contains(
                "- inst-def  [restarting]  -  restart always (launcher root), 0 restarts, attempt \
                 3, last exit: exit code 1, next start in 60 s"
            ),
            "{out}"
        );

        // A failed instance past its cap names the cap; one the daemon
        // refused names the refused attempt; one that never restarted names
        // no attempt. A restarting instance whose wait has passed names what
        // it waits for.
        response.instances[0].state = InstanceState::Failed;
        response.instances[0].restart_status.attempt = 6;
        response.instances[1].restart_status.next_start_ms = Some(now - 1);
        let out = rendered(&response);
        assert!(
            out.contains(
                "2 restarts, attempt 6 refused: max_restarts 5 reached, failed starts included, \
                 last exit: signal 9"
            ),
            "{out}"
        );
        assert!(
            out.contains(
                "next start due; it waits for the last exit's notices to other daemons, or for a \
                 stack launch, build, join or removal or a `peppy node remove` in progress on \
                 this daemon"
            ),
            "{out}"
        );
        // The attempt at the cap was taken, so a refusal of it is the
        // daemon's.
        response.instances[0].restart_status.attempt = 5;
        let out = rendered(&response);
        assert!(
            out.contains("2 restarts, attempt 5 refused, last exit: signal 9"),
            "{out}"
        );
        response.instances[0].restart_status.attempt = 4;
        let out = rendered(&response);
        assert!(
            out.contains("2 restarts, attempt 4 refused, last exit: signal 9"),
            "{out}"
        );
        // The wait is rounded up to whole seconds.
        for (left, shown) in [(1, "1 s"), (1_500, "2 s"), (2_000, "2 s")] {
            response.instances[1].restart_status.next_start_ms = Some(now + left);
            let out = rendered(&response);
            assert!(out.contains(&format!("next start in {shown}")), "{out}");
        }
        response.instances[0].restart_status.attempt = 0;
        let out = rendered(&response);
        assert!(
            out.contains("2 restarts, last exit: signal 9"),
            "a failed instance that never took an attempt names none: {out}"
        );
        response.instances[0].restart_status.restart_count = 1;
        let out = rendered(&response);
        assert!(out.contains("1 restart, last exit"), "{out}");
    }

    #[test]
    fn print_node_info_renders_stage_instances_and_logs() {
        let response = sample_response();
        let mut out = String::new();
        format_node_info(&mut out, &response, None);

        assert!(
            out.contains("Stage:     Ready"),
            "stage line missing:\n{out}"
        );
        assert!(
            out.contains("Instances: 2 tracked"),
            "instance count missing:\n{out}"
        );
        assert!(
            out.contains("- inst-abc  [running]  healthy"),
            "running healthy instance line missing:\n{out}"
        );
        assert!(
            out.contains("- inst-def  [starting]  unhealthy"),
            "starting unhealthy instance line missing:\n{out}"
        );

        // Logs section grouped under <name>:<tag>
        assert!(out.contains("\nLogs\n"), "Logs heading missing:\n{out}");
        assert!(
            out.contains("sensor_node:v1"),
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
    fn print_node_info_omits_logs_when_none_present() {
        let mut response = sample_response();
        response.add_log_path = None;
        response.run_log_paths.clear();

        let mut out = String::new();
        format_node_info(&mut out, &response, None);

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
        format_node_info(&mut out, &response, None);

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
        format_node_info(&mut out, &response, None);

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
        format_node_info(&mut out, &response, None);

        assert!(
            out.contains("  tags: array[string]"),
            "array parameter should render with item type:\n{out}"
        );
    }

    /// Every interface section lists its non-empty kinds in a fixed order,
    /// each under its own heading.
    #[test]
    fn print_node_info_renders_dependencies_and_interface_sections() {
        let out = format_with_config(
            r#"{
                peppy_schema: "node/v1",
                manifest: {
                    name: "relay",
                    tag: "v1",
                    description: ["A test node."],
                    implements: [{ name: "rgb_camera", tag: "v1", link_id: "camera" }],
                    depends_on: {
                        nodes: [
                            { name: "camera_node", tag: "v1", link_id: "source" },
                            { name: "config_node", tag: "v1", link_id: "config" },
                            { name: "nav_node", tag: "v1", link_id: "nav" },
                        ],
                    },
                },
                interfaces: {
                    topics: {
                        emits: [
                            { name: "joint_state", description: "A test topic.", qos_profile: "sensor_data", message_format: { position: "f64" } },
                            { link_id: "camera", name: "video_stream" },
                        ],
                        consumes: [
                            { link_id: "source", name: "video_stream" },
                            { link_id: "source", name: "depth_stream" },
                        ],
                    },
                    services: {
                        exposes: [{ name: "get_status", description: "A test service." }],
                        consumes: [{ link_id: "config", name: "get_config" }],
                    },
                    actions: {
                        exposes: [{ name: "calibrate", description: "A test action." }],
                        consumes: [{ link_id: "nav", name: "go_to_pose" }],
                    },
                },
                execution: { language: "rust", run_cmd: ["./bin"] },
            }"#,
        );

        let rule = "-".repeat(50);
        let expected = [
            "",
            "Dependencies",
            &rule,
            "  - config",
            "  - nav",
            "  - source",
            "",
            "Exposed Interfaces",
            &rule,
            "Emitted Topics:",
            "  - joint_state (qos: SensorData)",
            "  - video_stream (via implements slot: camera)",
            "Services:",
            "  - get_status",
            "Actions:",
            "  - calibrate",
            "",
            "Consumed Interfaces",
            &rule,
            "Consumed Topics:",
            "  - video_stream (via slot: source)",
            "  - depth_stream (via slot: source)",
            "Services:",
            "  - get_config (via slot: config)",
            "Actions:",
            "  - go_to_pose (via slot: nav)",
            "",
            "Integrity",
        ]
        .join("\n");
        assert!(out.contains(&format!("\n{expected}\n")), "{out}");
    }

    /// An interface section with only empty lists is left out, and a kind
    /// with an empty list gets no line under a section that is shown.
    #[test]
    fn print_node_info_omits_empty_interface_lists() {
        let out = format_with_config(
            r#"{
                peppy_schema: "node/v1",
                manifest: { name: "quiet", tag: "v1", description: ["A test node."] },
                interfaces: {
                    topics: { emits: [], consumes: [] },
                    services: { exposes: [{ name: "get_status", description: "A test service." }], consumes: [] },
                    actions: { exposes: [], consumes: [] },
                },
                execution: { language: "rust", run_cmd: ["./bin"] },
            }"#,
        );

        let rule = "-".repeat(50);
        let expected = [
            "    - inst-def: /tmp/peppy/logs/run/inst-def.log",
            "",
            "Exposed Interfaces",
            &rule,
            "Services:",
            "  - get_status",
            "",
            "Integrity",
        ]
        .join("\n");
        assert!(out.contains(&format!("{expected}\n")), "{out}");
    }
}
