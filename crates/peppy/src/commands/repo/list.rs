use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use core_node_api::encoding::{RepoListNodeEntry, RepoListRepoEntry, RepoListRequest};

use crate::commands::CALLER_INSTANCE_ID;
use crate::context::AppContext;
use crate::error::{Error, Result};
use peppylib::core_node::transport::poll;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn list_repos(ctx: &Arc<AppContext>) -> Result<()> {
    crate::commands::block_on(list_repos_async(ctx))
}

async fn list_repos_async(ctx: &Arc<AppContext>) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    let response = poll(
        &RepoListRequest,
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        &conn.target_core_node,
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

    if response.repos.is_empty() {
        println!("No repositories configured. Run `peppy repo init` to add the defaults.");
        return Ok(());
    }

    // The repository that wins each identity is the first one listed for
    // it: the daemon emits nodes in repository id order, and the lower id
    // wins resolution.
    let mut winner: HashMap<(&str, &str), &str> = HashMap::new();
    for node in &response.nodes {
        winner
            .entry((node.node_name.as_str(), node.node_tag.as_str()))
            .or_insert(node.repo_label.as_str());
    }

    let colorize = crate::terminal::colors_enabled();
    for (index, repo) in response.repos.iter().enumerate() {
        let nodes: Vec<&RepoListNodeEntry> = response
            .nodes
            .iter()
            .filter(|n| n.repo_id == repo.id)
            .collect();
        if index > 0 {
            println!();
        }
        print_repo_header(repo, nodes.len(), colorize);
        print_nodes(&nodes, &winner, colorize);
    }

    if response.nodes.is_empty() {
        println!();
        println!("No nodes found. Run `peppy repo refresh` to discover nodes.");
    }

    Ok(())
}

fn print_repo_header(repo: &RepoListRepoEntry, node_count: usize, colorize: bool) {
    let mut header = format!(
        "{} ({} {} nodes):",
        repo.label,
        node_count,
        repo.source_type.as_str()
    );
    if repo.retained {
        // Retention is the whole point of containment, but entries kept
        // from an earlier read are not current and saying so is what
        // stops a machine sitting on stale bytes believing otherwise.
        let when = repo
            .last_read_unix_secs
            .map(format_timestamp)
            .unwrap_or_else(|| "never".to_owned());
        header.push_str(&paint(
            &format!("  [retained, last read {when}]"),
            "\x1b[38;5;208m",
            colorize,
        ));
    }
    println!("{header}");

    if let Some(failure) = &repo.failure {
        println!(
            "{}",
            paint(
                &format!(
                    "  last refresh failed ({}): {}",
                    failure.kind, failure.detail
                ),
                "\x1b[31m",
                colorize,
            )
        );
    }
}

fn print_nodes(nodes: &[&RepoListNodeEntry], winner: &HashMap<(&str, &str), &str>, colorize: bool) {
    let max_name_len = nodes.iter().map(|n| n.node_name.len()).max().unwrap_or(0);
    let max_tag_len = nodes.iter().map(|n| n.node_tag.len()).max().unwrap_or(0);

    for node in nodes {
        // Two situations that used to share one "(duplicate)" label.
        // Shadowing resolves deterministically and is a feature; a
        // conflict has no winner and does not resolve at all. Reading the
        // second as the first is what let the original defect hide.
        let suffix = if node.conflict {
            paint("  (conflict: claimed twice here)", "\x1b[31m", colorize)
        } else if node.duplicate {
            let by = winner
                .get(&(node.node_name.as_str(), node.node_tag.as_str()))
                .copied()
                .unwrap_or("a higher-priority repository");
            paint(&format!("  (shadowed by {by})"), "\x1b[38;5;208m", colorize)
        } else {
            String::new()
        };
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

fn paint(text: &str, colour: &str, colorize: bool) -> String {
    if colorize {
        format!("{colour}{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

/// Renders a unix timestamp in local time, to the minute. Out-of-range
/// values render as the raw number rather than being dropped: a status
/// file is a diagnostic, so showing something odd beats showing nothing.
fn format_timestamp(unix_secs: u64) -> String {
    i64::try_from(unix_secs)
        .ok()
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| format!("unix {unix_secs}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_renders_to_the_minute() {
        // Compare against the same conversion rather than a fixed string,
        // so the test does not depend on the host's timezone.
        let expected = chrono::DateTime::from_timestamp(1_753_900_000, 0)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string();
        assert_eq!(format_timestamp(1_753_900_000), expected);
    }

    #[test]
    fn out_of_range_timestamp_degrades_to_the_raw_value() {
        assert_eq!(format_timestamp(u64::MAX), format!("unix {}", u64::MAX));
    }

    #[test]
    fn paint_is_a_no_op_without_colour() {
        assert_eq!(paint("  (conflict)", "\x1b[31m", false), "  (conflict)");
        assert_eq!(
            paint("  (conflict)", "\x1b[31m", true),
            "\x1b[31m  (conflict)\x1b[0m"
        );
    }
}
