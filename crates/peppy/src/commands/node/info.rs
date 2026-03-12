use config::AnyType;
use core_node::encoding::{NodeInfoRequest, NodeInfoResponse};
use peppylib::MessengerHandle;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use super::source::parse_node_source;
use crate::context::AppContext;
use crate::error::{Error, Result};

const CALLER_INSTANCE_ID: &str = "peppy-cli";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub fn node_info(ctx: &Arc<AppContext>, source: String, git_ref: Option<String>) -> Result<()> {
    crate::commands::block_on(node_info_async(ctx, source, git_ref))
}

async fn node_info_async(
    ctx: &Arc<AppContext>,
    source: String,
    git_ref: Option<String>,
) -> Result<()> {
    let daemon_state = ctx.read_daemon_state()?;
    let core_node_name = daemon_state.core_node_name.clone();

    let node_source = parse_node_source(&source, git_ref)?;

    let messenger = MessengerHandle::from_host_port(
        config::consts::DEFAULT_MESSAGING_HOST,
        daemon_state.messaging_port,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("Failed to connect to daemon: {}", e)))?;

    let request = NodeInfoRequest::new(node_source);
    let response = request
        .poll(
            &messenger,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            REQUEST_TIMEOUT,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to get node info: {}", e)))?;

    print_node_info(&response);
    Ok(())
}

fn print_node_info(response: &NodeInfoResponse) {
    let config = &response.config;
    let manifest = &config.manifest;

    println!();
    println!("Node Information");
    println!("{}", "=".repeat(50));
    println!();

    // Basic info
    println!("Name:      {}", manifest.name.as_str());
    println!("Tag:       {}", manifest.tag);
    println!("Language:  {:?}", manifest.language);

    // Labels
    if let Some(labels) = &manifest.labels
        && !labels.is_empty()
    {
        println!("Labels:    {}", labels.join(", "));
    }

    // Commands
    if let Some(build) = &config.process {
        if let Some(add_cmd) = &build.add_cmd {
            println!("Add cmd:   {}", add_cmd.join(" "));
        }
        println!("Start cmd: {}", build.start_cmd.join(" "));
    }
    if let Some(container) = &config.container {
        println!("Container: {}", container.def_file);
    }

    // Node stack status
    println!();
    println!("Node Stack Status");
    println!("{}", "-".repeat(50));
    if response.is_in_node_stack {
        println!("Status:    In node stack");
        if response.instances_names.is_empty() {
            println!("Instances: None running");
        } else {
            println!("Instances: {} running", response.instances_names.len());
            for instance in &response.instances_names {
                println!("           - {}", instance);
            }
        }
    } else {
        println!("Status:    Not in node stack");
    }

    // Dependencies (extracted from consumes interfaces)
    {
        let mut dependencies: BTreeSet<&str> = BTreeSet::new();

        if let Some(topics) = &config.interfaces.topics
            && let Some(expected) = &topics.expects
        {
            for topic in expected {
                dependencies.insert(&topic.node);
            }
        }
        if let Some(services) = &config.interfaces.services
            && let Some(consumed) = &services.consumes
        {
            for service in consumed {
                dependencies.insert(&service.node);
            }
        }
        if let Some(actions) = &config.interfaces.actions
            && let Some(consumed) = &actions.consumes
        {
            for action in consumed {
                if !action.node.is_empty() {
                    dependencies.insert(&action.node);
                }
            }
        }

        if !dependencies.is_empty() {
            println!();
            println!("Dependencies");
            println!("{}", "-".repeat(50));
            for dep in &dependencies {
                println!("  - {}", dep);
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
            println!();
            println!("Exposed Interfaces");
            println!("{}", "-".repeat(50));

            if let Some(topics) = config
                .interfaces
                .topics
                .as_ref()
                .and_then(|t| t.emits.as_ref())
                && !topics.is_empty()
            {
                println!("Emitted Topics:");
                for topic in topics {
                    println!("  - {} (qos: {:?})", topic.name, topic.qos_profile);
                }
            }

            if let Some(services) = config
                .interfaces
                .services
                .as_ref()
                .and_then(|s| s.exposes.as_ref())
                && !services.is_empty()
            {
                println!("Services:");
                for service in services {
                    println!("  - {}", service.name);
                }
            }

            if let Some(actions) = config
                .interfaces
                .actions
                .as_ref()
                .and_then(|a| a.exposes.as_ref())
                && !actions.is_empty()
            {
                println!("Actions:");
                for action in actions {
                    println!("  - {}", action.name);
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
            .and_then(|t| t.expects.as_ref())
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
            println!();
            println!("Consumed Interfaces");
            println!("{}", "-".repeat(50));

            if let Some(topics) = config
                .interfaces
                .topics
                .as_ref()
                .and_then(|t| t.expects.as_ref())
                && !topics.is_empty()
            {
                println!("Expected Topics:");
                for topic in topics {
                    println!(
                        "  - {} (from {}:{})",
                        topic.id.as_str(),
                        topic.node,
                        topic.name
                    );
                }
            }

            if let Some(services) = config
                .interfaces
                .services
                .as_ref()
                .and_then(|s| s.consumes.as_ref())
                && !services.is_empty()
            {
                println!("Services:");
                for service in services {
                    println!(
                        "  - {} (from {}:{})",
                        service.id.as_str(),
                        service.node,
                        service.name
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
                println!("Actions:");
                for action in actions {
                    println!(
                        "  - {} (from {}:{})",
                        action.id.as_str(),
                        action.node,
                        action.name
                    );
                }
            }
        }
    }

    // Integrity
    println!();
    println!("Integrity");
    println!("{}", "-".repeat(50));
    println!("Config SHA256: {}", response.config_integrity);

    // Parameters
    if !config.parameters.is_empty() {
        println!();
        println!("Parameters");
        println!("{}", "-".repeat(50));
        for (key, value) in &config.parameters {
            println!("  {}: {}", key, format_any_type(value));
        }
    }

    println!();
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
