use config::AnyType;
use gix_url::Url as GitUrl;
use master_node::encoding::{NodeInfoRequest, NodeInfoResponse, NodeSource};
use peppylib::MessengerHandle;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::context::AppContext;
use crate::error::{Error, Result};

const CALLER_INSTANCE_ID: &str = "peppy-cli";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

fn is_probably_remote_source(source: &str) -> bool {
    source.contains("://") || source.starts_with("git@")
}

fn is_supported_http_archive(url: &url::Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    path.ends_with(".tar.zst") || path.ends_with(".tar.zstd") || path.ends_with(".tzst")
}

fn parse_git_repo_url_and_path(source: &str) -> Result<(GitUrl, String)> {
    if let Ok(mut parsed) = url::Url::parse(source) {
        parsed.set_query(None);
        parsed.set_fragment(None);

        let segments: Vec<&str> = parsed
            .path_segments()
            .map(|segments| segments.collect())
            .unwrap_or_default();
        let repo_index = segments
            .iter()
            .rposition(|segment| segment.ends_with(".git"));

        let (repo_url_str, repo_path) = if let Some(repo_index) = repo_index {
            let repo_path_segments = segments[..=repo_index].join("/");
            let repo_relative_path = segments[repo_index + 1..].join("/");

            let mut repo_url = parsed.clone();
            repo_url.set_path(&format!("/{}", repo_path_segments));
            (repo_url.to_string(), repo_relative_path)
        } else {
            (parsed.to_string(), String::new())
        };

        let repo_url = GitUrl::try_from(repo_url_str.as_str()).map_err(|e| {
            Error::ExecutionFailed(format!("Invalid git URL '{}': {}", repo_url_str, e))
        })?;
        return Ok((repo_url, repo_path));
    }

    let (repo_url_str, repo_path) = if let Some((before, after)) = source.split_once(".git/") {
        (format!("{before}.git"), after.to_string())
    } else {
        (source.to_string(), String::new())
    };

    let repo_url = GitUrl::try_from(repo_url_str.as_str()).map_err(|e| {
        Error::ExecutionFailed(format!("Invalid git URL '{}': {}", repo_url_str, e))
    })?;
    Ok((repo_url, repo_path))
}

fn parse_node_source(source: &str, git_ref: Option<String>) -> Result<NodeSource> {
    if is_probably_remote_source(source) {
        if let Ok(url) = url::Url::parse(source)
            && matches!(url.scheme(), "http" | "https")
            && is_supported_http_archive(&url)
        {
            if git_ref.is_some() {
                return Err(Error::ExecutionFailed(
                    "`--ref` is only supported for git sources".to_string(),
                ));
            }
            return Ok(NodeSource::Http { url });
        }

        let (repo_url, repo_path) = parse_git_repo_url_and_path(source)?;
        return Ok(NodeSource::Git {
            repo_url,
            repo_path,
            repo_ref: git_ref,
        });
    }

    if git_ref.is_some() {
        return Err(Error::ExecutionFailed(
            "`--ref` is only supported for git sources".to_string(),
        ));
    }

    let source_path = PathBuf::from(source);
    let peppy_json5 = if source_path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json5"))
    {
        source_path
    } else {
        source_path.join("peppy.json5")
    };

    let peppy_json5 = peppy_json5.canonicalize().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to resolve path '{}': {}",
            peppy_json5.display(),
            e
        ))
    })?;

    let from_dir = peppy_json5
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    Ok(NodeSource::Fs(from_dir))
}

pub fn node_info(ctx: &Arc<AppContext>, source: String, git_ref: Option<String>) -> Result<()> {
    crate::commands::block_on(node_info_async(ctx, source, git_ref))
}

async fn node_info_async(
    ctx: &Arc<AppContext>,
    source: String,
    git_ref: Option<String>,
) -> Result<()> {
    let daemon_state = ctx.read_daemon_state().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to read daemon state. Is the peppy daemon running? Error: {}",
            e
        ))
    })?;
    let master_node_name = daemon_state.master_node_name.clone();

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
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
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
    if let Some(labels) = &manifest.labels {
        if !labels.is_empty() {
            println!("Labels:    {}", labels.join(", "));
        }
    }

    // Commands
    if let Some(add_cmd) = &manifest.add_cmd {
        println!("Add cmd:   {}", add_cmd.join(" "));
    }
    println!("Start cmd: {}", manifest.start_cmd.join(" "));

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

    // Dependencies (extracted from subscribes_to interfaces)
    if let Some(subscribes) = config.interfaces.subscribes_to.as_ref() {
        let mut dependencies: BTreeSet<&str> = BTreeSet::new();

        if let Some(topics) = &subscribes.topics {
            for topic in topics {
                dependencies.insert(&topic.node);
            }
        }
        if let Some(services) = &subscribes.services {
            for service in services {
                dependencies.insert(&service.node);
            }
        }
        if let Some(actions) = &subscribes.actions {
            for action in actions {
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

    // Interfaces
    if let Some(interfaces) = config.interfaces.exposes.as_ref() {
        let has_content = interfaces.topics.as_ref().is_some_and(|t| !t.is_empty())
            || interfaces.services.as_ref().is_some_and(|s| !s.is_empty())
            || interfaces.actions.as_ref().is_some_and(|a| !a.is_empty());

        if has_content {
            println!();
            println!("Exposed Interfaces");
            println!("{}", "-".repeat(50));

            if let Some(topics) = &interfaces.topics {
                if !topics.is_empty() {
                    println!("Topics:");
                    for topic in topics {
                        println!("  - {} (qos: {:?})", topic.name, topic.qos_profile);
                    }
                }
            }

            if let Some(services) = &interfaces.services {
                if !services.is_empty() {
                    println!("Services:");
                    for service in services {
                        println!("  - {}", service.name);
                    }
                }
            }

            if let Some(actions) = &interfaces.actions {
                if !actions.is_empty() {
                    println!("Actions:");
                    for action in actions {
                        println!("  - {}", action.name);
                    }
                }
            }
        }
    }

    if let Some(subscribes) = config.interfaces.subscribes_to.as_ref() {
        let has_content = subscribes.topics.as_ref().is_some_and(|t| !t.is_empty())
            || subscribes.services.as_ref().is_some_and(|s| !s.is_empty())
            || subscribes.actions.as_ref().is_some_and(|a| !a.is_empty());

        if has_content {
            println!();
            println!("Subscribed Interfaces");
            println!("{}", "-".repeat(50));

            if let Some(topics) = &subscribes.topics {
                if !topics.is_empty() {
                    println!("Topics:");
                    for topic in topics {
                        println!(
                            "  - {} (from {}:{})",
                            topic.id.as_str(),
                            topic.node,
                            topic.name
                        );
                    }
                }
            }

            if let Some(services) = &subscribes.services {
                if !services.is_empty() {
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
            }

            if let Some(actions) = &subscribes.actions {
                if !actions.is_empty() {
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
    }

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
