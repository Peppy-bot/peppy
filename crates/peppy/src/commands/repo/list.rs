use std::sync::Arc;
use std::time::Duration;

use core_node::encoding::{RepoListNodeEntry, RepoListRequest};

use crate::commands::CALLER_INSTANCE_ID;
use crate::context::AppContext;
use crate::error::{Error, Result};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn list_repos(ctx: &Arc<AppContext>) -> Result<()> {
    crate::commands::block_on(list_repos_async(ctx))
}

async fn list_repos_async(ctx: &Arc<AppContext>) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    let response = RepoListRequest
        .poll(
            conn.messenger,
            &conn.core_node_name,
            CALLER_INSTANCE_ID,
            &conn.core_node_name,
            REQUEST_TIMEOUT,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to list repositories: {}", e)))?;

    if !response.success {
        return Err(Error::ExecutionFailed(format!(
            "Failed to list repositories: {}",
            response.error_message.unwrap_or_default()
        )));
    }

    if response.nodes.is_empty() {
        println!("No nodes found. Run `peppy repo refresh` to discover nodes.");
        return Ok(());
    }

    let mut local_nodes: Vec<&RepoListNodeEntry> = Vec::new();
    let mut git_nodes: Vec<&RepoListNodeEntry> = Vec::new();
    let mut http_nodes: Vec<&RepoListNodeEntry> = Vec::new();

    for node in &response.nodes {
        match node.source_type.as_str() {
            "fs" => local_nodes.push(node),
            "git" => git_nodes.push(node),
            "url" => http_nodes.push(node),
            _ => local_nodes.push(node),
        }
    }

    if !local_nodes.is_empty() {
        println!("Local ({}):", local_nodes.len());
        print_nodes(&local_nodes);
    }

    if !git_nodes.is_empty() {
        if !local_nodes.is_empty() {
            println!();
        }
        println!("Git ({}):", git_nodes.len());
        print_nodes(&git_nodes);
    }

    if !http_nodes.is_empty() {
        if !local_nodes.is_empty() || !git_nodes.is_empty() {
            println!();
        }
        println!("HTTP ({}):", http_nodes.len());
        print_nodes(&http_nodes);
    }

    Ok(())
}

fn print_nodes(nodes: &[&RepoListNodeEntry]) {
    let max_name_len = nodes.iter().map(|n| n.node_name.len()).max().unwrap_or(0);
    let max_tag_len = nodes.iter().map(|n| n.node_tag.len()).max().unwrap_or(0);

    for node in nodes {
        if node.variants.is_empty() {
            println!(
                "  {:<name_w$}  {:<tag_w$}  {}",
                node.node_name,
                node.node_tag,
                node.path,
                name_w = max_name_len,
                tag_w = max_tag_len,
            );
        } else {
            println!(
                "  {:<name_w$}  {:<tag_w$}  {}  [variants: {}]",
                node.node_name,
                node.node_tag,
                node.path,
                node.variants.join(", "),
                name_w = max_name_len,
                tag_w = max_tag_len,
            );
        }
    }
}
