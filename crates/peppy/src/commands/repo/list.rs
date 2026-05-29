use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;

use core_node_api::encoding::{RepoListNodeEntry, RepoListRequest};

use crate::commands::CALLER_INSTANCE_ID;
use crate::context::AppContext;
use crate::error::{Error, Result};
use peppylib::core_node::transport::poll_repo_list;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn list_repos(ctx: &Arc<AppContext>) -> Result<()> {
    crate::commands::block_on(list_repos_async(ctx))
}

async fn list_repos_async(ctx: &Arc<AppContext>) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    let response = poll_repo_list(
        &RepoListRequest,
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

    let mut first_section = true;
    let mut start = 0;
    while start < response.nodes.len() {
        let repo_id = response.nodes[start].repo_id;
        let mut end = start + 1;
        while end < response.nodes.len() && response.nodes[end].repo_id == repo_id {
            end += 1;
        }
        let group: Vec<&RepoListNodeEntry> = response.nodes[start..end].iter().collect();
        if !first_section {
            println!();
        }
        first_section = false;
        let head = group[0];
        println!(
            "{} ({} {} nodes):",
            head.repo_label,
            group.len(),
            head.source_type.as_str()
        );
        print_nodes(&group);
        start = end;
    }

    Ok(())
}

fn print_nodes(nodes: &[&RepoListNodeEntry]) {
    let max_name_len = nodes.iter().map(|n| n.node_name.len()).max().unwrap_or(0);
    let max_tag_len = nodes.iter().map(|n| n.node_tag.len()).max().unwrap_or(0);
    let is_tty = std::io::stdout().is_terminal();

    for node in nodes {
        let mut suffix = String::new();
        if node.duplicate {
            if is_tty {
                suffix.push_str("  \x1b[38;5;208m(duplicate)\x1b[0m");
            } else {
                suffix.push_str("  (duplicate)");
            }
        }
        println!(
            "  {:<name_w$}  {:<tag_w$}  {}{}",
            node.node_name,
            node.node_tag,
            node.path,
            suffix,
            name_w = max_name_len,
            tag_w = max_tag_len,
        );
    }
}
