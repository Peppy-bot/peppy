use std::io::IsTerminal as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use config::runtime::SlotBinding;
use core_node_api::encoding::StackListRequest;
use core_node_api::{
    InstanceState, SerializedEdge, SerializedInstance, SerializedNode, SerializedNodeGraph,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

    let mut out = format_stack_list(&nodes, &edges, colors_enabled());

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
/// it can be unit-tested directly. `colorize` tints node labels, instances,
/// and bindings with ANSI SGR codes; the caller passes `false` for
/// non-interactive output so piped/redirected text and tests stay plain.
pub fn format_stack_list(
    nodes: &[SerializedNode],
    edges: &[SerializedEdge],
    colorize: bool,
) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(out, "Node stack");
    let _ = writeln!(out);

    if nodes.is_empty() {
        let _ = writeln!(out, "  (empty)");
        let _ = writeln!(out);
    } else {
        render_nodes_table(&mut out, nodes, colorize);
    }

    // Per-instance bindings. A distinct view from the node table above, but it
    // rides the same `graph_json` payload — each instance now carries its
    // resolved slot bindings — so there is no extra wire round-trip.
    let _ = writeln!(out, "Instance bindings");
    let _ = writeln!(out);
    let binding_nodes: Vec<&SerializedNode> =
        nodes.iter().filter(|n| !n.instances.is_empty()).collect();
    if binding_nodes.is_empty() {
        let _ = writeln!(out, "  (none)");
        let _ = writeln!(out);
    } else {
        render_bindings_table(&mut out, &binding_nodes, colorize);
    }

    // Dependencies reuse node labels but a distinct `➔` arrow (bindings above
    // use a lighter `→`) so the two relationships never read as the same edge.
    let _ = writeln!(out, "Dependencies");
    if edges.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        for edge in edges {
            let _ = writeln!(
                out,
                "  {} ➔ {}",
                paint(colorize, NODE_COLOR, &edge.from.label()),
                paint(colorize, NODE_COLOR, &edge.to.label()),
            );
        }
    }

    out
}

/// Whether `peppy stack list` should emit ANSI colors: only when stdout is an
/// interactive terminal and `NO_COLOR` is unset/empty. Mirrors the gate in
/// `terminal.rs` so the CLI stays consistent.
fn colors_enabled() -> bool {
    std::io::stdout().is_terminal() && !no_color_requested()
}

fn no_color_requested() -> bool {
    std::env::var("NO_COLOR").is_ok_and(|v| !v.is_empty())
}

// ANSI SGR codes used to tint the tables, applied only when `colorize` is set.
// `col_width` strips these before measuring, so a colored cell occupies the
// same display columns as its plain text and the box stays aligned.
const NODE_COLOR: &str = "\x1b[36m"; // cyan — node labels
const COUNT_COLOR: &str = "\x1b[32m"; // green — per-node instance counts
const INSTANCE_COLOR: &str = "\x1b[35m"; // magenta — instance ids
const BINDING_COLOR: &str = "\x1b[33m"; // yellow — slot bindings
const RESET: &str = "\x1b[0m";

/// Wraps `s` in `code`/reset when `colorize` is set, otherwise returns it
/// unchanged. Empty input is left untouched so blank continuation cells don't
/// carry dangling escape codes.
fn paint(colorize: bool, code: &str, s: &str) -> String {
    if colorize && !s.is_empty() {
        format!("{code}{s}{RESET}")
    } else {
        s.to_string()
    }
}

/// Terminal display width of a cell. Column widths, box-drawing borders, and
/// cell padding must all measure the same way or the table skews on non-ASCII
/// content — a wide CJK glyph or a Unicode path in the `PATH` column counts as
/// more bytes than display columns, and a combining mark as fewer. Routing
/// every measurement through this keeps the three in agreement.
///
/// ANSI SGR escapes (the color codes `paint` injects) occupy zero display
/// columns, so they are skipped here; otherwise a colored cell would measure
/// wider than its plain text and skew the box against the borders.
fn col_width(s: &str) -> usize {
    if !s.as_bytes().contains(&0x1b) {
        return UnicodeWidthStr::width(s);
    }
    let mut width = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Consume a CSI sequence: `\x1b[` params … up to a final byte in
            // `@`..=`~`. The `[` introducer itself falls in that range, so it
            // must be skipped first or the scan would stop one char too early.
            // These bytes never reach the screen as columns.
            if chars.clone().next() == Some('[') {
                chars.next();
            }
            for f in chars.by_ref() {
                if ('@'..='~').contains(&f) {
                    break;
                }
            }
        } else {
            width += UnicodeWidthChar::width(c).unwrap_or(0);
        }
    }
    width
}

/// Column headers kept in one place so widths stay consistent between the
/// separator and data rows.
const HEADERS: [&str; 4] = ["NODE", "STAGE", "INSTANCES", "PATH"];

fn render_nodes_table(out: &mut String, nodes: &[SerializedNode], colorize: bool) {
    use std::fmt::Write as _;

    let rows: Vec<[String; 4]> = nodes
        .iter()
        .map(|n| {
            [
                paint(colorize, NODE_COLOR, &n.label()),
                n.stage_label().to_string(),
                paint(colorize, COUNT_COLOR, &format_instances_compact(n)),
                display_path(n),
            ]
        })
        .collect();

    let mut widths: [usize; 4] = HEADERS.map(col_width);
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(col_width(cell));
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

/// Headers for the per-instance bindings table. The node→instance→binding
/// hierarchy is conveyed by row grouping rather than nesting columns: a node
/// label appears only on the first row of its group, an instance id only on
/// the first of its binding rows, and a horizontal rule separates node groups.
const BINDING_HEADERS: [&str; 3] = ["NODE", "INSTANCE", "BINDINGS"];

/// Renders the per-instance bindings table. `nodes` must already be filtered
/// to entries with at least one instance — the caller prints `(none)` when
/// none qualify, so this never emits an empty body.
fn render_bindings_table(out: &mut String, nodes: &[&SerializedNode], colorize: bool) {
    use std::fmt::Write as _;

    // One block of rows per node. Within a block, the NODE cell is populated
    // only on the first row and each instance's INSTANCE cell only on the
    // first of its binding rows; the rest are blank continuation cells.
    let blocks: Vec<Vec<[String; 3]>> = nodes
        .iter()
        .map(|node| {
            let mut rows: Vec<[String; 3]> = Vec::new();
            let mut node_cell = paint(colorize, NODE_COLOR, &node.label());
            for instance in &node.instances {
                let mut instance_cell = paint(colorize, INSTANCE_COLOR, &instance.instance_id);
                for binding in format_instance_bindings(instance, colorize) {
                    rows.push([
                        std::mem::take(&mut node_cell),
                        std::mem::take(&mut instance_cell),
                        binding,
                    ]);
                }
            }
            rows
        })
        .collect();

    let mut widths: [usize; 3] = BINDING_HEADERS.map(col_width);
    for row in blocks.iter().flatten() {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(col_width(cell));
        }
    }

    write_border(out, &widths, '┌', '┬', '┐');
    let header_row: [String; 3] = BINDING_HEADERS.map(|h| h.to_string());
    write_row(out, &header_row, &widths);
    write_border(out, &widths, '├', '┼', '┤');
    for (group_idx, block) in blocks.iter().enumerate() {
        // Rule between node groups so it is unambiguous which instances belong
        // to which node, even when a node's last instance has only one binding.
        if group_idx > 0 {
            write_border(out, &widths, '├', '┼', '┤');
        }
        for row in block {
            write_row(out, row, &widths);
        }
    }
    write_border(out, &widths, '└', '┴', '┘');
    let _ = writeln!(out);
}

/// One display string per slot binding on the instance, in `link_id →
/// producer` form and ordered by link id (`BTreeMap` iteration order). Each
/// side is tinted by what it denotes: the link id in the binding color, the
/// producer in the instance color (it names another instance), so the line is
/// readable by hue rather than by column. The `→` arrow is deliberately
/// lighter than the `➔` used for dependencies so the two relationships read
/// differently. Returns `["(none)"]` when the instance has no bindings so its
/// row still renders.
fn format_instance_bindings(instance: &SerializedInstance, colorize: bool) -> Vec<String> {
    if instance.slot_bindings.is_empty() {
        return vec!["(none)".to_string()];
    }
    instance
        .slot_bindings
        .iter()
        .map(|(link_id, binding)| {
            format!(
                "{} → {}",
                paint(colorize, BINDING_COLOR, link_id),
                paint(colorize, INSTANCE_COLOR, &format_slot_binding(binding)),
            )
        })
        .collect()
}

/// Right-hand side of a `link_id -> …` binding line: the producer instance the
/// slot resolves to. A `from_any` slot with explicit producers lists them
/// comma-separated; a `from_any` slot left bindless — and the degenerate
/// "bound to nothing" case — render as `(any)`.
fn format_slot_binding(binding: &SlotBinding) -> String {
    match binding {
        SlotBinding::Pinned {
            producer_instance_id,
        } => producer_instance_id.clone(),
        SlotBinding::FromAnyBound {
            producer_instance_ids,
        } if !producer_instance_ids.is_empty() => producer_instance_ids.join(", "),
        // `FromAnyBound` with no producers and `FromAnyUnbound` are both "no
        // pinned producer"; collapse them so the line never trails as
        // `link_id -> ` with an empty right-hand side.
        SlotBinding::FromAnyBound { .. } | SlotBinding::FromAnyUnbound => "(any)".to_string(),
    }
}

fn write_border(out: &mut String, widths: &[usize], left: char, sep: char, right: char) {
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

fn write_row(out: &mut String, cells: &[String], widths: &[usize]) {
    use std::fmt::Write as _;
    let _ = write!(out, "│");
    for (cell, w) in cells.iter().zip(widths.iter()) {
        // Pad by display columns, not `char` count: `{:<width$}` would
        // mis-pad wide/zero-width glyphs and skew the box against the
        // `col_width`-based widths and borders.
        let pad = w.saturating_sub(col_width(cell));
        let _ = write!(out, " {}{} │", cell, " ".repeat(pad));
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
                    slot_bindings: std::collections::BTreeMap::new(),
                })
                .collect(),
        }
    }

    /// `(instance_id, state, [(slot, binding)])` rows fed to [`binding_node`].
    type InstanceSpec<'a> = (&'a str, InstanceState, Vec<(&'a str, SlotBinding)>);

    /// Like [`node`] but lets each instance carry slot bindings, for
    /// exercising the bindings table. Always `Ready`/`v1`.
    fn binding_node(name: &str, instances: Vec<InstanceSpec<'_>>) -> SerializedNode {
        SerializedNode {
            name: name.to_string(),
            tag: "v1".to_string(),
            config_path: format!("/tmp/{}.json5", name),
            artifact_path: None,
            stage: Some(NodeStage::Ready),
            instances: instances
                .into_iter()
                .map(|(id, state, binds)| SerializedInstance {
                    instance_id: id.to_string(),
                    state,
                    slot_bindings: binds
                        .into_iter()
                        .map(|(slot, binding)| (slot.to_string(), binding))
                        .collect(),
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
        let out = format_stack_list(&nodes, &[], false);

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
        let out = format_stack_list(&nodes, &[], false);
        assert!(
            out.contains("2 (1 running, 1 starting)"),
            "mixed breakdown missing:\n{}",
            out
        );
    }

    #[test]
    fn empty_stack_renders_empty_marker() {
        let out = format_stack_list(&[], &[], false);
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
        let out = format_stack_list(&[from, to], &edges, false);
        assert!(
            out.contains("brain:v1 ➔ sensor:v1"),
            "edge line missing:\n{}",
            out
        );
    }

    /// Slice of the rendered output covering only the "Instance bindings"
    /// section, so assertions don't accidentally match the node table or the
    /// dependency list (both of which also use `->`).
    fn bindings_section(out: &str) -> &str {
        let start = out
            .find("Instance bindings")
            .expect("Instance bindings heading missing");
        let rest = &out[start..];
        let end = rest.find("Dependencies").unwrap_or(rest.len());
        &rest[..end]
    }

    #[test]
    fn bindings_table_renders_headers_and_per_instance_binding() {
        let nodes = vec![binding_node(
            "backbone",
            vec![(
                "bk-1",
                InstanceState::Running,
                vec![(
                    "arm",
                    SlotBinding::Pinned {
                        producer_instance_id: "arm-1".to_string(),
                    },
                )],
            )],
        )];
        let out = format_stack_list(&nodes, &[], false);
        let section = bindings_section(&out);

        for header in BINDING_HEADERS {
            assert!(section.contains(header), "missing header {header}:\n{out}");
        }
        assert!(
            section.contains("backbone:v1"),
            "node label missing:\n{out}"
        );
        assert!(section.contains("bk-1"), "instance id missing:\n{out}");
        assert!(
            section.contains("arm → arm-1"),
            "binding line missing:\n{out}"
        );
    }

    #[test]
    fn bindings_table_lists_each_binding_on_its_own_row_under_one_instance() {
        let nodes = vec![binding_node(
            "brain",
            vec![(
                "br-1",
                InstanceState::Running,
                // Inserted out of sorted order so the assertion below fails if
                // the BTreeMap link-id sort is ever dropped.
                vec![
                    (
                        "clock",
                        SlotBinding::Pinned {
                            producer_instance_id: "clk-1".to_string(),
                        },
                    ),
                    (
                        "backbone",
                        SlotBinding::Pinned {
                            producer_instance_id: "bb-1".to_string(),
                        },
                    ),
                ],
            )],
        )];
        let out = format_stack_list(&nodes, &[], false);
        let section = bindings_section(&out);

        // Both slots render, sorted by link id (BTreeMap order) regardless of
        // insertion order: "backbone" precedes "clock".
        let backbone_at = section
            .find("backbone → bb-1")
            .expect("first binding missing");
        let clock_at = section
            .find("clock → clk-1")
            .expect("second binding missing");
        assert!(
            backbone_at < clock_at,
            "bindings should be sorted by link id:\n{out}"
        );
        // The instance id appears once: subsequent binding rows reuse a blank
        // continuation cell rather than repeating the id.
        assert_eq!(
            section.matches("br-1").count(),
            1,
            "instance id should appear on exactly one row:\n{out}"
        );
    }

    #[test]
    fn bindings_table_renders_none_for_instance_without_bindings() {
        let nodes = vec![binding_node(
            "camera",
            vec![("cam-1", InstanceState::Running, vec![])],
        )];
        let out = format_stack_list(&nodes, &[], false);
        let section = bindings_section(&out);

        assert!(section.contains("cam-1"), "instance id missing:\n{out}");
        assert!(
            section.contains("(none)"),
            "bindless instance should render (none):\n{out}"
        );
    }

    #[test]
    fn bindings_table_renders_from_any_variants() {
        let nodes = vec![binding_node(
            "nav",
            vec![(
                "nav-1",
                InstanceState::Running,
                vec![
                    (
                        "sensors",
                        SlotBinding::FromAnyBound {
                            producer_instance_ids: vec!["cam-1".to_string(), "cam-2".to_string()],
                        },
                    ),
                    ("extra", SlotBinding::FromAnyUnbound),
                ],
            )],
        )];
        let out = format_stack_list(&nodes, &[], false);
        let section = bindings_section(&out);

        let sensors_at = section
            .find("sensors → cam-1, cam-2")
            .expect("from_any bound producers should be comma-joined");
        let extra_at = section
            .find("extra → (any)")
            .expect("from_any unbound should render (any)");
        // Sorted by link id regardless of insertion order: "extra" < "sensors".
        assert!(
            extra_at < sensors_at,
            "bindings should be sorted by link id:\n{out}"
        );
    }

    #[test]
    fn bindings_section_renders_none_when_no_node_has_instances() {
        let nodes = vec![node("sensor", "v1", NodeStage::Added, vec![])];
        let out = format_stack_list(&nodes, &[], false);
        let section = bindings_section(&out);
        assert!(
            section.contains("(none)"),
            "bindings section should be (none) when no instances exist:\n{out}"
        );
        // The bindings table headers must not appear when there is nothing to show.
        assert!(
            !section.contains("BINDINGS"),
            "no bindings table should render when no instances exist:\n{out}"
        );
    }

    #[test]
    fn bindings_table_renders_any_for_from_any_bound_without_producers() {
        // Defensive: a `FromAnyBound` carrying no producers (only reachable via
        // a hand-crafted / corrupt payload) must not render a dangling
        // `slot -> ` with an empty right-hand side.
        let nodes = vec![binding_node(
            "nav",
            vec![(
                "nav-1",
                InstanceState::Running,
                vec![(
                    "sensors",
                    SlotBinding::FromAnyBound {
                        producer_instance_ids: vec![],
                    },
                )],
            )],
        )];
        let out = format_stack_list(&nodes, &[], false);
        let section = bindings_section(&out);

        assert!(
            section.contains("sensors → (any)"),
            "empty from_any bound should collapse to (any):\n{out}"
        );
        assert!(
            !section.contains("sensors → \n") && !section.contains("sensors →  "),
            "binding line must not trail with an empty producer:\n{out}"
        );
    }

    #[test]
    fn bindings_table_groups_multiple_nodes_with_separators_and_drops_instanceless_nodes() {
        let nodes = vec![
            binding_node(
                "alpha",
                vec![
                    (
                        "alpha-1",
                        InstanceState::Running,
                        vec![(
                            "dep",
                            SlotBinding::Pinned {
                                producer_instance_id: "beta-1".to_string(),
                            },
                        )],
                    ),
                    // Second instance, no bindings: stays inside alpha's group
                    // with no separator before it.
                    ("alpha-2", InstanceState::Running, vec![]),
                ],
            ),
            // No instances -> must be filtered out of the bindings table.
            node("ghost", "v1", NodeStage::Added, vec![]),
            binding_node("beta", vec![("beta-1", InstanceState::Running, vec![])]),
        ];
        let out = format_stack_list(&nodes, &[], false);
        let section = bindings_section(&out);

        // Instance-less node never appears in the bindings table.
        assert!(
            !section.contains("ghost:v1"),
            "instance-less node should be filtered out:\n{out}"
        );

        // Each present node label appears exactly once — on its group's first
        // row — proving the node-cell continuation blanking across a real group
        // boundary (not just within a single node).
        assert_eq!(
            section.matches("alpha:v1").count(),
            1,
            "alpha label should appear once:\n{out}"
        );
        assert_eq!(
            section.matches("beta:v1").count(),
            1,
            "beta label should appear once:\n{out}"
        );

        // Two instance-bearing groups -> the header rule plus exactly one
        // inter-group rule: two lines beginning with the left-tee `├`. (The
        // two instance rows within alpha's group get no rule between them.)
        let rule_lines = section.lines().filter(|l| l.starts_with('├')).count();
        assert_eq!(
            rule_lines, 2,
            "expected exactly one separator between the two node groups:\n{out}"
        );
    }

    #[test]
    fn tables_stay_aligned_with_wide_glyph_cells() {
        // Double-width CJK glyphs span two display columns but more than two
        // bytes, so a byte-length or `char`-count measure would skew the box.
        // Every box-drawing line within a table must share one display width.
        let nodes = vec![binding_node(
            "机器人",
            vec![(
                "实例-uno",
                InstanceState::Running,
                vec![(
                    "传感器",
                    SlotBinding::Pinned {
                        producer_instance_id: "相机-1".to_string(),
                    },
                )],
            )],
        )];
        let out = format_stack_list(&nodes, &[], false);

        fn box_line_widths(block: &str) -> Vec<usize> {
            block
                .lines()
                .filter(|l| matches!(l.chars().next(), Some('┌' | '├' | '└' | '│')))
                .map(UnicodeWidthStr::width)
                .collect()
        }
        fn assert_uniform(label: &str, widths: &[usize]) {
            assert!(
                widths.len() >= 2 && widths.iter().all(|w| *w == widths[0]),
                "{label} box lines have mismatched display widths {widths:?}"
            );
        }

        // Node table region (between the "Node stack" and "Instance bindings"
        // headings) and the bindings table region must each be internally aligned.
        let node_region = &out[..out.find("Instance bindings").expect("bindings heading")];
        assert_uniform("node table", &box_line_widths(node_region));
        assert_uniform("bindings table", &box_line_widths(bindings_section(&out)));
    }

    /// Drops ANSI SGR escape sequences so a colored render can be compared
    /// against its plain counterpart.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                if chars.clone().next() == Some('[') {
                    chars.next();
                }
                for f in chars.by_ref() {
                    if ('@'..='~').contains(&f) {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn colorize_is_purely_additive() {
        // Colorizing must only inject SGR codes — it must never move a column.
        // Stripping the codes back out has to reproduce the plain render byte
        // for byte, which also exercises the ANSI-aware `col_width`.
        let nodes = vec![binding_node(
            "backbone",
            vec![(
                "bk-1",
                InstanceState::Running,
                vec![(
                    "arm",
                    SlotBinding::Pinned {
                        producer_instance_id: "arm-1".to_string(),
                    },
                )],
            )],
        )];
        let from = node("brain", "v1", NodeStage::Ready, vec![]);
        let to = node("sensor", "v1", NodeStage::Ready, vec![]);
        let edges = vec![SerializedEdge {
            from: from.clone(),
            to: to.clone(),
        }];

        let plain = format_stack_list(&nodes, &edges, false);
        let colored = format_stack_list(&nodes, &edges, true);

        assert!(
            colored.contains('\x1b'),
            "colorized output should carry ANSI codes:\n{colored:?}"
        );
        assert!(
            !plain.contains('\x1b'),
            "plain output must stay free of ANSI codes:\n{plain:?}"
        );
        assert_eq!(
            strip_ansi(&colored),
            plain,
            "stripping colors must reproduce the plain layout exactly"
        );

        // The two relationships use distinct arrows so they never read alike:
        // `→` for bindings, `➔` for dependencies.
        assert!(
            bindings_section(&plain).contains("arm → arm-1"),
            "bindings should use the light arrow:\n{plain}"
        );
        assert!(
            plain.contains("brain:v1 ➔ sensor:v1"),
            "dependencies should use the heavy arrow:\n{plain}"
        );
    }
}
