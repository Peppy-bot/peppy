use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use core_node_api::encoding::StackListRequest;
use core_node_api::{InstanceState, SerializedEdge, SerializedNode, SerializedNodeGraph};

use crate::commands::CALLER_INSTANCE_ID;
use crate::context::AppContext;
use crate::error::{Error, Result};

use peppylib::core_node::transport::poll_stack_list;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub fn list_nodes(ctx: &Arc<AppContext>, dot_graph_path: Option<PathBuf>) -> Result<()> {
    let output = crate::commands::block_on(list_nodes_collecting(ctx, dot_graph_path))?;
    print!("{}", output);
    Ok(())
}

/// Like [`list_nodes`] but returns the rendered output as a `String` instead
/// of printing it. Used by integration tests so they can assert against the
/// exact bytes the CLI would print without having to capture stdout.
pub async fn list_nodes_collecting(
    ctx: &Arc<AppContext>,
    dot_graph_path: Option<PathBuf>,
) -> Result<String> {
    let conn = ctx.connect_to_daemon().await?;

    let response = poll_stack_list(
        &StackListRequest::new(dot_graph_path.is_some()),
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        &conn.core_node_name,
        REQUEST_TIMEOUT,
    )
    .await?;

    let graph: SerializedNodeGraph = serde_json::from_str(&response.graph_json)
        .map_err(|e| Error::ExecutionFailed(format!("failed to parse stack graph JSON: {e}")))?;

    // Sort nodes by label for consistent output, with the daemon root first.
    let mut nodes = graph.nodes;
    nodes.sort_by(|a, b| {
        let a_is_daemon = a.label().starts_with(&conn.core_node_name);
        let b_is_daemon = b.label().starts_with(&conn.core_node_name);
        match (a_is_daemon, b_is_daemon) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.label().cmp(&b.label()),
        }
    });

    // Sort edges by (from_label, to_label) for consistent output.
    let mut edges = graph.edges;
    edges.sort_by(|a, b| {
        let a_key = (a.from.label(), a.to.label());
        let b_key = (b.from.label(), b.to.label());
        a_key.cmp(&b_key)
    });

    let mut out = format_stack_list(&nodes, &edges);

    if let (Some(path), Some(dot_graph)) = (dot_graph_path, response.dot_graph) {
        std::fs::write(&path, dot_graph).map_err(|e| {
            Error::ExecutionFailed(format!(
                "Failed to write DOT graph to {}: {}",
                path.display(),
                e
            ))
        })?;
        use std::fmt::Write as _;
        let _ = writeln!(out, "DOT graph saved to {}", path.display());
    }

    Ok(out)
}

/// Pure formatter for the `peppy stack list` output — kept free of any IO so
/// it can be unit-tested directly.
pub fn format_stack_list(nodes: &[SerializedNode], edges: &[SerializedEdge]) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(out, "Node stack");
    let _ = writeln!(out);

    if nodes.is_empty() {
        let _ = writeln!(out, "  (empty)");
    } else {
        render_nodes_table(&mut out, nodes);
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "Dependencies");
    if edges.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        for edge in edges {
            let _ = writeln!(out, "  {} -> {}", edge.from.label(), edge.to.label());
        }
    }

    out
}

/// Column headers kept in one place so widths stay consistent between the
/// separator and data rows.
const HEADERS: [&str; 4] = ["NODE", "STAGE", "INSTANCES", "PATH"];

fn render_nodes_table(out: &mut String, nodes: &[SerializedNode]) {
    use std::fmt::Write as _;

    let rows: Vec<[String; 4]> = nodes
        .iter()
        .map(|n| {
            [
                n.label(),
                n.stage_label().to_string(),
                format_instances_compact(n),
                display_path(n),
            ]
        })
        .collect();

    let mut widths: [usize; 4] = HEADERS.map(|h| h.len());
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            if cell.len() > widths[i] {
                widths[i] = cell.len();
            }
        }
    }

    write_border(out, &widths, '┌', '┬', '┐');
    let header_row: [String; 4] = HEADERS.map(|h| h.to_string());
    write_row(out, &header_row, &widths);
    write_border(out, &widths, '├', '┼', '┤');
    for row in &rows {
        write_row(out, row, &widths);
    }
    write_border(out, &widths, '└', '┴', '┘');
    let _ = writeln!(out);
}

fn write_border(out: &mut String, widths: &[usize; 4], left: char, sep: char, right: char) {
    use std::fmt::Write as _;
    let _ = write!(out, "{}", left);
    for (i, w) in widths.iter().enumerate() {
        for _ in 0..(w + 2) {
            let _ = write!(out, "─");
        }
        let _ = write!(out, "{}", if i + 1 == widths.len() { right } else { sep });
    }
    let _ = writeln!(out);
}

fn write_row(out: &mut String, cells: &[String; 4], widths: &[usize; 4]) {
    use std::fmt::Write as _;
    let _ = write!(out, "│");
    for (cell, w) in cells.iter().zip(widths.iter()) {
        let _ = write!(out, " {:<width$} │", cell, width = w);
    }
    let _ = writeln!(out);
}

/// Compact per-node instance summary. Detailed per-instance info is
/// intentionally deferred to `peppy node info` — the list view is meant to
/// fit one node per row.
fn format_instances_compact(node: &SerializedNode) -> String {
    if node.instances.is_empty() {
        return "0".to_string();
    }
    let running = node
        .instances
        .iter()
        .filter(|i| i.state == InstanceState::Running)
        .count();
    let starting = node
        .instances
        .iter()
        .filter(|i| i.state == InstanceState::Starting)
        .count();
    match (running, starting) {
        (r, 0) => format!("{} running", r),
        (0, s) => format!("{} starting", s),
        (r, s) => format!("{} ({} running, {} starting)", r + s, r, s),
    }
}

/// For nodes that have been built, the artifact path is the most useful
/// locator; otherwise fall back to the source config path.
fn display_path(node: &SerializedNode) -> String {
    node.artifact_path
        .clone()
        .unwrap_or_else(|| node.config_path.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_node_api::{NodeStage, SerializedInstance};

    fn node(
        name: &str,
        tag: &str,
        stage: NodeStage,
        instances: Vec<(&str, InstanceState)>,
    ) -> SerializedNode {
        SerializedNode {
            name: name.to_string(),
            tag: tag.to_string(),
            config_path: format!("/tmp/{}.json5", name),
            artifact_path: None,
            stage: Some(stage),
            instances: instances
                .into_iter()
                .map(|(id, state)| SerializedInstance {
                    instance_id: id.to_string(),
                    state,
                    link_ids: Vec::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn table_renders_headers_and_rows() {
        let nodes = vec![
            node("sensor", "v1", NodeStage::Added, vec![]),
            node(
                "brain",
                "v1",
                NodeStage::Ready,
                vec![("i1", InstanceState::Running)],
            ),
        ];
        let out = format_stack_list(&nodes, &[]);

        for header in HEADERS {
            assert!(out.contains(header), "missing header {}:\n{}", header, out);
        }
        assert!(out.contains("sensor:v1"), "missing sensor row:\n{}", out);
        assert!(out.contains("brain:v1"), "missing brain row:\n{}", out);
        assert!(
            out.contains("1 running"),
            "instances column missing:\n{}",
            out
        );
        // Never prefix table output with [INFO].
        assert!(
            !out.contains("[INFO]"),
            "output must not contain [INFO]:\n{}",
            out
        );
    }

    #[test]
    fn mixed_running_and_starting_instances_render_with_breakdown() {
        let nodes = vec![node(
            "brain",
            "v1",
            NodeStage::Ready,
            vec![
                ("r1", InstanceState::Running),
                ("s1", InstanceState::Starting),
            ],
        )];
        let out = format_stack_list(&nodes, &[]);
        assert!(
            out.contains("2 (1 running, 1 starting)"),
            "mixed breakdown missing:\n{}",
            out
        );
    }

    #[test]
    fn empty_stack_renders_empty_marker() {
        let out = format_stack_list(&[], &[]);
        assert!(out.contains("(empty)"), "empty marker missing:\n{}", out);
        assert!(
            out.contains("Dependencies"),
            "deps heading missing:\n{}",
            out
        );
        assert!(out.contains("(none)"), "deps none marker missing:\n{}", out);
    }

    #[test]
    fn edges_render_as_arrows() {
        let from = node("brain", "v1", NodeStage::Ready, vec![]);
        let to = node("sensor", "v1", NodeStage::Ready, vec![]);
        let edges = vec![SerializedEdge {
            from: from.clone(),
            to: to.clone(),
        }];
        let out = format_stack_list(&[from, to], &edges);
        assert!(
            out.contains("brain:v1 -> sensor:v1"),
            "edge line missing:\n{}",
            out
        );
    }
}
