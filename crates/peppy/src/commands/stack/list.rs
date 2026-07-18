use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use crate::commands::CALLER_INSTANCE_ID;
use crate::context::AppContext;
use crate::error::{Error, Result};
use config::runtime::PairingSlotBinding;
use core_node_api::encoding::StackListRequest;
use core_node_api::{InstanceState, NodeStage, SerializedEdge, SerializedInstance, SerializedNode};
use futures::future::join_all;

use peppylib::{CoreNodePresenceMessenger, core_node::transport::poll};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// One independently queried core-node stack. Query and decode failures stay
/// in their section so a disappearing daemon does not hide healthy peers.
#[derive(Debug)]
pub struct StackSection {
    /// Attributed from the response's self-reported identity; falls back to
    /// the queried name when the query failed.
    pub core_node: String,
    /// Self-reported generation of the daemon that answered (`None` when the
    /// query failed). During an active name collision this identifies which
    /// claimant served the section.
    pub instance_id: Option<String>,
    pub host_name: String,
    /// Live instance ids advertising this name at enumeration time; more than
    /// one means an active collision. Zero when no live token was found.
    pub live_claimants: usize,
    pub outcome: std::result::Result<(Vec<SerializedNode>, Vec<SerializedEdge>), String>,
}

/// Rendered `stack list` output plus the core nodes whose section failed.
/// Callers own the failure policy: the CLI prints the output and exits
/// non-zero on any failed name, tests assert on the fields directly.
pub struct StackListReport {
    pub output: String,
    pub failed_names: Vec<String>,
}

pub fn list_nodes(ctx: &Arc<AppContext>) -> Result<()> {
    let colorize = crate::terminal::colors_enabled();
    let report = crate::commands::block_on(list_nodes_collecting(ctx, colorize))?;
    print!("{}", report.output);
    if report.failed_names.is_empty() {
        Ok(())
    } else {
        Err(Error::ExecutionFailed(format!(
            "stack list failed for: {}",
            report.failed_names.join(", ")
        )))
    }
}

/// Like [`list_nodes`] but returns the [`StackListReport`] instead of printing
/// and deciding the exit status. `colorize` is passed in rather than read from
/// the ambient terminal so the result is deterministic: the CLI passes
/// [`crate::terminal::colors_enabled`], while integration tests pass `false`
/// for stable, color-free assertions.
pub async fn list_nodes_collecting(
    ctx: &Arc<AppContext>,
    colorize: bool,
) -> Result<StackListReport> {
    let conn = ctx.connect_to_daemon().await?;

    let live = CoreNodePresenceMessenger::list_live(
        conn.messenger,
        conn.target_is_override
            .then_some(conn.target_core_node.as_str()),
        CoreNodePresenceMessenger::LIST_TIMEOUT,
    )
    .await?;
    let targets = if conn.target_is_override {
        vec![(
            conn.target_core_node.clone(),
            live_instance_count(&conn.target_core_node, live),
        )]
    } else {
        ordered_targets(&conn.core_node_name, live)
    };

    let sections = join_all(targets.into_iter().map(|(core_node, live_claimants)| {
        let messenger = conn.messenger;
        let caller_core_node = &conn.core_node_name;
        async move {
            let response = poll(
                &StackListRequest::new(),
                messenger,
                caller_core_node,
                CALLER_INSTANCE_ID,
                &core_node,
                REQUEST_TIMEOUT,
            )
            .await;

            match response {
                Ok(response) => StackSection {
                    // Attribute the section to the identity the daemon
                    // self-reports, not the name the request targeted; an
                    // empty name (daemon predating identity self-reporting)
                    // falls back to the target.
                    core_node: if response.core_node.is_empty() {
                        core_node
                    } else {
                        response.core_node
                    },
                    instance_id: (!response.instance_id.is_empty()).then_some(response.instance_id),
                    host_name: response.host_name,
                    live_claimants,
                    outcome: crate::commands::parse_stack_graph(&response.graph_json)
                        .map(|mut graph| {
                            sort_graph(&mut graph.nodes, &mut graph.edges);
                            (graph.nodes, graph.edges)
                        })
                        .map_err(|error| error.to_string()),
                },
                Err(error) => StackSection {
                    core_node,
                    instance_id: None,
                    host_name: "unknown".to_string(),
                    live_claimants,
                    outcome: Err(error.to_string()),
                },
            }
        }
    }))
    .await;

    let failed_names = sections
        .iter()
        .filter(|section| section.outcome.is_err())
        .map(|section| section.core_node.clone())
        .collect();

    Ok(StackListReport {
        output: format_stack_list(&sections, colorize),
        failed_names,
    })
}

fn ordered_targets(
    local_core_node: &str,
    live: Vec<pmi::CoreNodePresence>,
) -> Vec<(String, usize)> {
    let mut claims: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for presence in live {
        claims
            .entry(presence.core_node)
            .or_default()
            .insert(presence.instance_id);
    }

    let mut targets = Vec::with_capacity(claims.len().max(1));
    let local_claimants = claims.remove(local_core_node).map_or(0, |ids| ids.len());
    targets.push((local_core_node.to_string(), local_claimants));
    targets.extend(claims.into_iter().map(|(name, ids)| (name, ids.len())));
    targets
}

fn live_instance_count(core_node: &str, live: Vec<pmi::CoreNodePresence>) -> usize {
    live.into_iter()
        .filter(|presence| presence.core_node == core_node)
        .map(|presence| presence.instance_id)
        .collect::<BTreeSet<_>>()
        .len()
}

fn sort_graph(nodes: &mut [SerializedNode], edges: &mut [SerializedEdge]) {
    nodes.sort_by(|a, b| {
        let a_is_daemon = a.stage == Some(NodeStage::Root);
        let b_is_daemon = b.stage == Some(NodeStage::Root);
        match (a_is_daemon, b_is_daemon) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.label().cmp(&b.label()),
        }
    });
    edges.sort_by_key(|edge| (edge.from.label(), edge.to.label()));
}

/// Pure multi-core-node formatter for `peppy stack list`. Each distinct name
/// keeps its graph, host annotation, duplicate-name warning, or query error in
/// a separate outer panel.
pub fn format_stack_list(sections: &[StackSection], colorize: bool) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    for (index, section) in sections.iter().enumerate() {
        if index > 0 {
            let _ = writeln!(out);
        }
        let header = format!(
            "Core node: {} (host: {})",
            paint(colorize, NODE_COLOR, &section.core_node),
            section.host_name
        );
        let mut body = String::new();
        if section.live_claimants > 1 {
            let _ = match &section.instance_id {
                Some(instance_id) => writeln!(
                    body,
                    "warning: {} live daemons currently claim this name; answered by instance {}",
                    section.live_claimants, instance_id
                ),
                None => writeln!(
                    body,
                    "warning: {} live daemons currently claim this name",
                    section.live_claimants
                ),
            };
            let _ = writeln!(body);
        }

        match &section.outcome {
            Ok((nodes, edges)) => {
                body.push_str(&format_stack_body(nodes, edges, colorize));
            }
            Err(error) => {
                let _ = writeln!(body, "error: {error}");
            }
        }

        render_section_panel(&mut out, &header, &body);
    }
    out
}

/// Encloses one core node's complete report in a panel. Nested table borders
/// remain intact, while the continuous outer edge makes ownership clear when
/// several independently queried stacks are printed together.
/// Formats the existing tables inside one core-node section. `colorize` tints
/// node labels, instances, and bindings; table width measurement strips those
/// codes so colored and plain layouts remain identical.
fn format_stack_body(nodes: &[SerializedNode], edges: &[SerializedEdge], colorize: bool) -> String {
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
    // rides the same `graph_json` payload (each instance now carries its
    // resolved slot bindings) so there is no extra wire round-trip.
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

    // Per-instance pairing slots. Only rendered when some tracked instance
    // declares `depends_on.pairings`; the `⇌` arrow marks the relationship as
    // bidirectional, unlike the one-way binding `→` above.
    let pairing_nodes: Vec<&SerializedNode> = nodes
        .iter()
        .filter(|n| n.instances.iter().any(|i| !i.pairing_slots.is_empty()))
        .collect();
    if !pairing_nodes.is_empty() {
        let _ = writeln!(out, "Instance pairings");
        let _ = writeln!(out);
        render_pairings_table(&mut out, &pairing_nodes, colorize);
    }

    // Dependencies reuse node labels but a distinct `➔` arrow (bindings above
    // use a lighter `→`) so the two relationships never read as the same edge.
    let _ = writeln!(out, "Dependencies");
    if edges.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        for edge in edges {
            // A contract-implementation edge is annotated with the contract it
            // routes through, so it reads distinctly from a direct node dep. The
            // interface name is tinted the same as the node labels it relates.
            let via = match &edge.via_contract {
                Some(iface) => format!(
                    " (via {} contract implementation)",
                    paint(colorize, NODE_COLOR, iface)
                ),
                None => String::new(),
            };
            let _ = writeln!(
                out,
                "  {} ➔ {}{}",
                paint(colorize, NODE_COLOR, &edge.from.label()),
                paint(colorize, NODE_COLOR, &edge.to.label()),
                via,
            );
        }
    }

    out
}

// The tables are tinted with the shared `stack` color palette so `stack list`
// and `stack benchmark` color the same things the same way. `col_width` strips
// these codes before measuring, so a colored cell occupies the same display
// columns as its plain text and the box stays aligned.
use super::colors::{
    BINDING_COLOR, COUNT_COLOR, HEALTH_HEALTHY_COLOR, HEALTH_UNHEALTHY_COLOR, INSTANCE_COLOR,
    NODE_COLOR, STATUS_FAILED_COLOR, STATUS_FINISHED_COLOR, STATUS_RUNNING_COLOR,
    STATUS_STARTING_COLOR, paint,
};
use super::table::{render_section_panel, render_table};

/// Column headers kept in one place so widths stay consistent between the
/// separator and data rows.
const HEADERS: [&str; 4] = ["NODE", "STAGE", "INSTANCES", "PATH"];

fn render_nodes_table(out: &mut String, nodes: &[SerializedNode], colorize: bool) {
    let rows: Vec<Vec<String>> = nodes
        .iter()
        .map(|n| {
            vec![
                paint(colorize, NODE_COLOR, &n.label()),
                n.stage_label().to_string(),
                paint(colorize, COUNT_COLOR, &format_instances_compact(n)),
                display_path(n),
            ]
        })
        .collect();

    // A single block: the nodes table has no internal group rules.
    render_table(out, &HEADERS, &[rows]);
}

/// Headers for the per-instance bindings table. The node→instance→binding
/// hierarchy is conveyed by row grouping rather than nesting columns: a node
/// label appears only on the first row of its group, an instance id (with its
/// status and health) only on the first of its binding rows, and a horizontal
/// rule separates node groups.
const BINDING_HEADERS: [&str; 5] = ["NODE", "INSTANCE", "STATUS", "HEALTH", "BINDINGS"];

/// Renders the per-instance bindings table. `nodes` must already be filtered
/// to entries with at least one instance; the caller prints `(none)` when
/// none qualify, so this never emits an empty body.
fn render_bindings_table(out: &mut String, nodes: &[&SerializedNode], colorize: bool) {
    // One block of rows per node, so `render_table` draws a rule between node
    // groups (keeping it unambiguous which instances belong to which node, even
    // when a node's last instance has a single binding). Within a block, the
    // NODE cell is populated only on the first row and each instance's INSTANCE,
    // STATUS, and HEALTH cells only on the first of its binding rows; the rest
    // are blank continuation cells.
    let blocks: Vec<Vec<Vec<String>>> = nodes
        .iter()
        .map(|node| {
            let mut rows: Vec<Vec<String>> = Vec::new();
            let mut node_cell = paint(colorize, NODE_COLOR, &node.label());
            for instance in &node.instances {
                let mut instance_cell = paint(colorize, INSTANCE_COLOR, &instance.instance_id);
                let mut status_cell = format_instance_status(instance, colorize);
                let mut health_cell = format_instance_health(instance, colorize);
                for binding in format_instance_bindings(instance, colorize) {
                    rows.push(vec![
                        std::mem::take(&mut node_cell),
                        std::mem::take(&mut instance_cell),
                        std::mem::take(&mut status_cell),
                        std::mem::take(&mut health_cell),
                        binding,
                    ]);
                }
            }
            rows
        })
        .collect();

    render_table(out, &BINDING_HEADERS, &blocks);
}

/// Headers for the per-instance pairings table; grouped like the bindings
/// table (node label on the first row of its group, instance id on the first
/// of its slot rows).
const PAIRING_HEADERS: [&str; 3] = ["NODE", "INSTANCE", "PAIRINGS"];

/// Renders the per-instance pairing-slot table. `nodes` must already be
/// filtered to entries with at least one instance carrying pairing slots.
fn render_pairings_table(out: &mut String, nodes: &[&SerializedNode], colorize: bool) {
    let blocks: Vec<Vec<Vec<String>>> = nodes
        .iter()
        .map(|node| {
            let mut rows: Vec<Vec<String>> = Vec::new();
            let mut node_cell = paint(colorize, NODE_COLOR, &node.label());
            for instance in &node.instances {
                if instance.pairing_slots.is_empty() {
                    continue;
                }
                let mut instance_cell = paint(colorize, INSTANCE_COLOR, &instance.instance_id);
                for line in format_instance_pairings(instance, colorize) {
                    rows.push(vec![
                        std::mem::take(&mut node_cell),
                        std::mem::take(&mut instance_cell),
                        line,
                    ]);
                }
            }
            rows
        })
        .collect();

    render_table(out, &PAIRING_HEADERS, &blocks);
}

/// One display string per pairing slot on the instance, ordered by link id:
/// `link_id ⇌ peer_instance:peer_link@core_node (pairing:tag)` while paired
/// (the `@core_node` suffix matches the bindings table's `instance@node`
/// producer style), `link_id ⇌ (unpaired) [role r of pairing:tag]` while
/// not; the role makes an unpaired row self-describing when composing a
/// `--pair` for it, and an `optional: true` slot is labelled
/// `(unpaired, optional)` so it never reads as a missing required peer.
fn format_instance_pairings(instance: &SerializedInstance, colorize: bool) -> Vec<String> {
    instance
        .pairing_slots
        .iter()
        .map(|(link_id, slot)| {
            let link = paint(colorize, BINDING_COLOR, link_id);
            match &slot.binding {
                PairingSlotBinding::Paired { peer, peer_link_id } => format!(
                    "{link} ⇌ {} ({}:{})",
                    paint(
                        colorize,
                        INSTANCE_COLOR,
                        &format!("{}:{}@{}", peer.instance_id, peer_link_id, peer.core_node),
                    ),
                    slot.pairing_name,
                    slot.pairing_tag,
                ),
                PairingSlotBinding::Unpaired => {
                    let state = if slot.optional {
                        "(unpaired, optional)"
                    } else {
                        "(unpaired)"
                    };
                    format!(
                        "{link} ⇌ {state} [role {} of {}:{}]",
                        slot.role, slot.pairing_name, slot.pairing_tag,
                    )
                }
            }
        })
        .collect()
}

/// The instance's lifecycle state for the STATUS column. Tinted as a
/// traffic-light cue (green once running, yellow while still starting, blue
/// once finished cleanly, red if it crashed) and rendered from the same
/// `InstanceState` that `peppy node info` shows as `[running]`/`[starting]`,
/// without the brackets since the column delimits it.
fn format_instance_status(instance: &SerializedInstance, colorize: bool) -> String {
    let color = match instance.state {
        InstanceState::Running => STATUS_RUNNING_COLOR,
        InstanceState::Starting => STATUS_STARTING_COLOR,
        InstanceState::Finished => STATUS_FINISHED_COLOR,
        InstanceState::Failed => STATUS_FAILED_COLOR,
    };
    paint(colorize, color, &instance.state.to_string())
}

/// The instance's health for the HEALTH column. For a live instance this is the
/// daemon's last `node_health` probe carried in [`SerializedInstance::healthy`]:
/// green for `healthy`, red for `unhealthy`, so a failing instance stands out.
/// A terminal (finished/failed) instance has exited, so it has no live health to
/// report and renders a neutral, uncolored `-`.
fn format_instance_health(instance: &SerializedInstance, colorize: bool) -> String {
    let label = crate::commands::instance_health_label(instance.state, instance.healthy);
    if instance.state.is_terminal() {
        return label.to_string();
    }
    let color = if instance.healthy {
        HEALTH_HEALTHY_COLOR
    } else {
        HEALTH_UNHEALTHY_COLOR
    };
    paint(colorize, color, label)
}

/// One display string per slot binding on the instance, in `link_id →
/// producers` form and ordered by link id (`BTreeMap` iteration order). Each
/// side is tinted by what it denotes: the link id in the binding color, the
/// producers in the instance color (they name other instances), so the line
/// is readable by hue rather than by column. The `→` arrow is deliberately
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
        .map(|(link_id, bound)| {
            format!(
                "{} → {}",
                paint(colorize, BINDING_COLOR, link_id),
                paint(colorize, INSTANCE_COLOR, &format_slot_binding(bound)),
            )
        })
        .collect()
}

/// Right-hand side of a `link_id -> …` binding line: the slot's bound
/// producer set in declaration order, each member rendered as
/// `instance_id@core_node` (the full wire address every binding carries)
/// and joined with commas. An empty set (a `zero_or_more` slot bound to
/// nothing) renders as `(empty set)` so it never reads as a missing row.
fn format_slot_binding(bound: &config::runtime::BoundProducers) -> String {
    if bound.is_empty() {
        return "(empty set)".to_string();
    }
    bound
        .iter()
        .map(|producer| format!("{}@{}", producer.instance_id, producer.core_node))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Compact per-node instance summary. Detailed per-instance info is
/// intentionally deferred to `peppy node info`; the list view is meant to
/// fit one node per row. Terminal instances (finished/crashed) are counted too,
/// so a one-shot node that has completed still shows up rather than reading as
/// `0` once its only instance exits.
fn format_instances_compact(node: &SerializedNode) -> String {
    if node.instances.is_empty() {
        return "0".to_string();
    }
    let count = |state: InstanceState| node.instances.iter().filter(|i| i.state == state).count();
    let running = count(InstanceState::Running);
    let starting = count(InstanceState::Starting);
    let finished = count(InstanceState::Finished);
    let failed = count(InstanceState::Failed);

    // The common, unambiguous cases keep their original compact phrasing; any
    // mix (including terminal instances) falls through to an explicit
    // per-state breakdown so no instance is silently dropped from the count.
    match (running, starting, finished, failed) {
        (r, 0, 0, 0) => format!("{r} running"),
        (0, s, 0, 0) => format!("{s} starting"),
        (0, 0, f, 0) => format!("{f} finished"),
        (0, 0, 0, x) => format!("{x} failed"),
        (r, s, f, x) => {
            let total = r + s + f + x;
            let mut parts = Vec::new();
            if r > 0 {
                parts.push(format!("{r} running"));
            }
            if s > 0 {
                parts.push(format!("{s} starting"));
            }
            if f > 0 {
                parts.push(format!("{f} finished"));
            }
            if x > 0 {
                parts.push(format!("{x} failed"));
            }
            format!("{total} ({})", parts.join(", "))
        }
    }
}

/// For nodes that have been built, the artifact path is the most useful
/// locator; otherwise fall back to the source config path. The home directory
/// is collapsed to `~` to keep the column narrow.
fn display_path(node: &SerializedNode) -> String {
    let path = node
        .artifact_path
        .clone()
        .unwrap_or_else(|| node.config_path.clone());
    shorten_home(&path)
}

/// Collapses a leading home-directory prefix to `~`, matching shell display.
/// Returns the path unchanged when the home dir can't be resolved.
fn shorten_home(path: &str) -> String {
    match dirs::home_dir().as_deref().and_then(|h| h.to_str()) {
        Some(home) => shorten_home_with(path, home),
        None => path.to_string(),
    }
}

/// Core of [`shorten_home`], split out so it can be tested without depending on
/// the ambient home directory. Only an exact home dir or one followed by a
/// separator is rewritten, so a sibling like `/home/user2/...` is never
/// mangled into `~2/...`.
fn shorten_home_with(path: &str, home: &str) -> String {
    // Trailing separators on the resolved home would break the boundary check
    // below; a home of `/` has no useful `~` form, so leave such paths as-is.
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return path.to_string();
    }
    if path == home {
        return "~".to_string();
    }
    if let Some(rest) = path.strip_prefix(home)
        && rest.starts_with('/')
    {
        return format!("~{rest}");
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::super::table::skip_csi;
    use super::*;
    use config::runtime::ProducerRef;
    use core_node_api::{NodeStage, SerializedInstance};
    use unicode_width::UnicodeWidthStr;

    fn node(
        name: &str,
        tag: &str,
        stage: NodeStage,
        instances: Vec<(&str, InstanceState)>,
    ) -> SerializedNode {
        SerializedNode {
            name: name.to_string(),
            tag: tag.to_string(),
            core_node: "core-a".to_string(),
            config_path: format!("/tmp/{}.json5", name),
            artifact_path: None,
            stage: Some(stage),
            instances: instances
                .into_iter()
                .map(|(id, state)| SerializedInstance {
                    instance_id: id.to_string(),
                    state,
                    healthy: true,
                    slot_bindings: std::collections::BTreeMap::new(),
                    pairing_slots: std::collections::BTreeMap::new(),
                })
                .collect(),
        }
    }

    /// `(instance_id, state, [(slot, producers)])` rows fed to
    /// [`binding_node`]; each slot carries its ordered bound set.
    type InstanceSpec<'a> = (&'a str, InstanceState, Vec<(&'a str, Vec<ProducerRef>)>);

    /// Like [`node`] but lets each instance carry slot bindings, for
    /// exercising the bindings table. Always `Ready`/`v1`.
    fn binding_node(name: &str, instances: Vec<InstanceSpec<'_>>) -> SerializedNode {
        SerializedNode {
            name: name.to_string(),
            tag: "v1".to_string(),
            core_node: "core-a".to_string(),
            config_path: format!("/tmp/{}.json5", name),
            artifact_path: None,
            stage: Some(NodeStage::Ready),
            instances: instances
                .into_iter()
                .map(|(id, state, binds)| SerializedInstance {
                    instance_id: id.to_string(),
                    state,
                    healthy: true,
                    slot_bindings: binds
                        .into_iter()
                        .map(|(slot, producers)| {
                            (
                                slot.to_string(),
                                config::runtime::BoundProducers::try_from(producers)
                                    .expect("test producer sets are duplicate-free"),
                            )
                        })
                        .collect(),
                    pairing_slots: std::collections::BTreeMap::new(),
                })
                .collect(),
        }
    }

    fn successful_section(core_node: &str, host_name: &str) -> StackSection {
        StackSection {
            core_node: core_node.to_string(),
            instance_id: Some("gen-1".to_string()),
            host_name: host_name.to_string(),
            live_claimants: 1,
            outcome: Ok((
                vec![node(core_node, "v1", NodeStage::Root, vec![])],
                Vec::new(),
            )),
        }
    }

    #[test]
    fn multi_daemon_sections_keep_input_order_and_are_individually_boxed() {
        let sections = vec![
            successful_section("z-local", "robot-local"),
            successful_section("a-remote", "robot-remote"),
        ];
        let out = format_stack_list(&sections, false);

        let local = out
            .find("Core node: z-local (host: robot-local)")
            .expect("local section header");
        let remote = out
            .find("Core node: a-remote (host: robot-remote)")
            .expect("remote section header");
        assert!(
            local < remote,
            "formatter must preserve caller ordering:\n{out}"
        );

        let panels: Vec<&str> = out.trim_end().split("\n\n").collect();
        assert_eq!(panels.len(), 2, "one outer panel per core node:\n{out}");
        for (panel, core_node) in panels.iter().zip(["z-local", "a-remote"]) {
            let lines: Vec<&str> = panel.lines().collect();
            assert!(
                lines
                    .first()
                    .is_some_and(|line| line.starts_with('┌') && line.ends_with('┐')),
                "panel for {core_node} is missing its top border:\n{panel}"
            );
            assert!(
                lines.get(1).is_some_and(|line| line
                    .starts_with(&format!("│ Core node: {core_node} "))
                    && line.ends_with(" │")),
                "panel for {core_node} is missing its header row:\n{panel}"
            );
            assert!(
                lines
                    .get(2)
                    .is_some_and(|line| line.starts_with('├') && line.ends_with('┤')),
                "panel for {core_node} is missing its header divider:\n{panel}"
            );
            assert!(
                lines
                    .last()
                    .is_some_and(|line| line.starts_with('└') && line.ends_with('┘')),
                "panel for {core_node} is missing its bottom border:\n{panel}"
            );
            assert!(
                lines.iter().any(|line| line.starts_with("│ Node stack")),
                "node stack escaped the panel for {core_node}:\n{panel}"
            );
            assert!(
                lines
                    .iter()
                    .any(|line| line.starts_with("│ Instance bindings")),
                "instance bindings escaped the panel for {core_node}:\n{panel}"
            );
            assert!(
                lines.iter().any(|line| line.starts_with("│ Dependencies")),
                "dependencies escaped the panel for {core_node}:\n{panel}"
            );

            let widths: Vec<usize> = lines
                .iter()
                .map(|line| UnicodeWidthStr::width(*line))
                .collect();
            assert!(
                widths.iter().all(|width| *width == widths[0]),
                "panel for {core_node} has mismatched display widths {widths:?}:\n{panel}"
            );
        }
    }

    #[test]
    fn duplicate_and_failed_sections_remain_visible() {
        let sections = vec![StackSection {
            core_node: "claimed".to_string(),
            instance_id: None,
            host_name: "unknown".to_string(),
            live_claimants: 3,
            outcome: Err("daemon disappeared".to_string()),
        }];
        let out = format_stack_list(&sections, false);
        assert!(out.contains("Core node: claimed (host: unknown)"));
        assert!(out.contains("warning: 3 live daemons currently claim this name"));
        assert!(out.contains("error: daemon disappeared"));
    }

    #[test]
    fn collision_warning_names_the_answering_instance() {
        let sections = vec![StackSection {
            live_claimants: 2,
            ..successful_section("claimed", "robo-a")
        }];
        let out = format_stack_list(&sections, false);
        assert!(
            out.contains(
                "warning: 2 live daemons currently claim this name; answered by instance gen-1"
            ),
            "collision warning should attribute the answering claimant:\n{out}"
        );
    }

    #[test]
    fn target_order_is_local_first_then_lexicographic_and_deduplicated() {
        let targets = ordered_targets(
            "z-local",
            vec![
                pmi::CoreNodePresence::new("b-remote", "b1"),
                pmi::CoreNodePresence::new("z-local", "local"),
                pmi::CoreNodePresence::new("a-remote", "a1"),
                pmi::CoreNodePresence::new("a-remote", "a2"),
            ],
        );
        assert_eq!(
            targets,
            vec![
                ("z-local".to_string(), 1),
                ("a-remote".to_string(), 2),
                ("b-remote".to_string(), 1),
            ]
        );
    }

    #[test]
    fn explicit_target_counts_distinct_live_instances_for_requested_name() {
        let count = live_instance_count(
            "requested",
            vec![
                pmi::CoreNodePresence::new("requested", "instance-a"),
                pmi::CoreNodePresence::new("requested", "instance-a"),
                pmi::CoreNodePresence::new("requested", "instance-b"),
                pmi::CoreNodePresence::new("other", "instance-c"),
            ],
        );
        assert_eq!(count, 2);
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
        let out = format_stack_body(&nodes, &[], false);

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
        let out = format_stack_body(&nodes, &[], false);
        assert!(
            out.contains("2 (1 running, 1 starting)"),
            "mixed breakdown missing:\n{}",
            out
        );
    }

    #[test]
    fn instances_compact_counts_a_finished_one_shot_instance() {
        // A one-shot node whose only instance has finished must still appear in
        // the INSTANCES column, not read as "0 running".
        let nodes = vec![node(
            "recorder",
            "v1",
            NodeStage::Ready,
            vec![("rec-1", InstanceState::Finished)],
        )];
        let out = format_stack_body(&nodes, &[], false);
        assert!(
            out.contains("1 finished"),
            "finished instance missing from compact count:\n{out}"
        );
    }

    #[test]
    fn instances_compact_breaks_down_a_mix_with_terminal_states() {
        let nodes = vec![node(
            "mix",
            "v1",
            NodeStage::Ready,
            vec![
                ("r1", InstanceState::Running),
                ("f1", InstanceState::Finished),
                ("x1", InstanceState::Failed),
            ],
        )];
        let out = format_stack_body(&nodes, &[], false);
        assert!(
            out.contains("3 (1 running, 1 finished, 1 failed)"),
            "mixed terminal breakdown missing:\n{out}"
        );
    }

    #[test]
    fn bindings_table_renders_terminal_state_with_neutral_health() {
        // A finished instance shows its terminal status and a neutral `-` for
        // health (no live probe), never a stale healthy/unhealthy verdict; a
        // failed instance reads as "failed", not "unhealthy".
        let nodes = vec![binding_node(
            "recorder",
            vec![
                ("rec-1", InstanceState::Finished, vec![]),
                ("rec-2", InstanceState::Failed, vec![]),
            ],
        )];
        let out = format_stack_body(&nodes, &[], false);
        let section = bindings_section(&out);

        let finished_line = section
            .lines()
            .find(|l| l.contains("rec-1"))
            .expect("finished instance row missing");
        assert!(
            finished_line.contains("finished"),
            "finished status missing:\n{out}"
        );
        assert!(
            !finished_line.contains("healthy"),
            "a finished instance must not render a health verdict:\n{out}"
        );

        let failed_line = section
            .lines()
            .find(|l| l.contains("rec-2"))
            .expect("failed instance row missing");
        assert!(
            failed_line.contains("failed"),
            "failed status missing:\n{out}"
        );
        assert!(
            !failed_line.contains("unhealthy"),
            "a failed instance must read as failed, not unhealthy:\n{out}"
        );
    }

    #[test]
    fn empty_stack_renders_empty_marker() {
        let out = format_stack_body(&[], &[], false);
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
            via_contract: None,
        }];
        let out = format_stack_body(&[from, to], &edges, false);
        assert!(
            out.contains("brain:v1 ➔ sensor:v1"),
            "edge line missing:\n{}",
            out
        );
    }

    #[test]
    fn contract_implementation_edge_renders_annotation() {
        let consumer = node("brain", "v1", NodeStage::Ready, vec![]);
        let provider = node("camera_mock", "v1", NodeStage::Ready, vec![]);
        let edges = vec![SerializedEdge {
            from: consumer.clone(),
            to: provider.clone(),
            via_contract: Some("uvc_camera:v1".to_string()),
        }];
        let out = format_stack_body(&[consumer, provider], &edges, false);
        assert!(
            out.contains("brain:v1 ➔ camera_mock:v1 (via uvc_camera:v1 contract implementation)"),
            "contract-implementation edge annotation missing:\n{}",
            out
        );
    }

    /// Slice of the rendered output covering only the "Instance bindings"
    /// section, so assertions don't accidentally match the node table, the
    /// pairings table, or the dependency list.
    fn bindings_section(out: &str) -> &str {
        let start = out
            .find("Instance bindings")
            .expect("Instance bindings heading missing");
        let rest = &out[start..];
        let end = rest
            .find("Instance pairings")
            .or_else(|| rest.find("Dependencies"))
            .unwrap_or(rest.len());
        &rest[..end]
    }

    /// Slice covering only the "Instance pairings" section, mirroring
    /// [`bindings_section`].
    fn pairings_section(out: &str) -> &str {
        let start = out
            .find("Instance pairings")
            .expect("Instance pairings heading missing");
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
                vec![("arm", vec![ProducerRef::new("core_a", "arm-1")])],
            )],
        )];
        let out = format_stack_body(&nodes, &[], false);
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
            section.contains("arm → arm-1@core_a"),
            "binding line missing:\n{out}"
        );
    }

    #[test]
    fn bindings_table_renders_status_column() {
        let nodes = vec![
            binding_node("arm", vec![("starting-1", InstanceState::Starting, vec![])]),
            binding_node(
                "brain",
                vec![(
                    "br-1",
                    InstanceState::Running,
                    vec![
                        ("camera", vec![ProducerRef::new("core_a", "cam-1")]),
                        ("controller", vec![ProducerRef::new("core_a", "ctl-1")]),
                    ],
                )],
            ),
        ];
        let out = format_stack_body(&nodes, &[], false);
        let section = bindings_section(&out);

        assert!(section.contains("STATUS"), "STATUS header missing:\n{out}");
        assert!(
            section.contains("starting"),
            "starting status missing:\n{out}"
        );
        // The status renders once per instance: br-1 has two binding rows but
        // its "running" status sits on the first only, blanked thereafter.
        assert_eq!(
            section.matches("running").count(),
            1,
            "status should appear once per instance:\n{out}"
        );
    }

    #[test]
    fn bindings_table_renders_health_column() {
        // A node with one healthy and one unhealthy instance, built directly so
        // both `SerializedInstance::healthy` values are exercised.
        let nodes = vec![SerializedNode {
            name: "arm".to_string(),
            tag: "v1".to_string(),
            core_node: "core-a".to_string(),
            config_path: "/tmp/arm.json5".to_string(),
            artifact_path: None,
            stage: Some(NodeStage::Ready),
            instances: vec![
                SerializedInstance {
                    instance_id: "healthy-1".to_string(),
                    state: InstanceState::Running,
                    healthy: true,
                    slot_bindings: std::collections::BTreeMap::new(),
                    pairing_slots: std::collections::BTreeMap::new(),
                },
                SerializedInstance {
                    instance_id: "down-1".to_string(),
                    state: InstanceState::Running,
                    healthy: false,
                    slot_bindings: std::collections::BTreeMap::new(),
                    pairing_slots: std::collections::BTreeMap::new(),
                },
            ],
        }];
        let out = format_stack_body(&nodes, &[], false);
        let section = bindings_section(&out);

        assert!(section.contains("HEALTH"), "HEALTH header missing:\n{out}");

        // Each instance reports its own health on its row: the healthy one reads
        // "healthy" (and not "unhealthy"), the failing one "unhealthy".
        let healthy_line = section
            .lines()
            .find(|l| l.contains("healthy-1"))
            .expect("healthy instance row missing");
        assert!(
            healthy_line.contains("healthy") && !healthy_line.contains("unhealthy"),
            "healthy instance should report healthy:\n{out}"
        );
        let down_line = section
            .lines()
            .find(|l| l.contains("down-1"))
            .expect("unhealthy instance row missing");
        assert!(
            down_line.contains("unhealthy"),
            "failing instance should report unhealthy:\n{out}"
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
                    ("clock", vec![ProducerRef::new("core_a", "clk-1")]),
                    ("backbone", vec![ProducerRef::new("core_a", "bb-1")]),
                ],
            )],
        )];
        let out = format_stack_body(&nodes, &[], false);
        let section = bindings_section(&out);

        // Both slots render, sorted by link id (BTreeMap order) regardless of
        // insertion order: "backbone" precedes "clock".
        let backbone_at = section
            .find("backbone → bb-1@core_a")
            .expect("first binding missing");
        let clock_at = section
            .find("clock → clk-1@core_a")
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
        let out = format_stack_body(&nodes, &[], false);
        let section = bindings_section(&out);

        assert!(section.contains("cam-1"), "instance id missing:\n{out}");
        assert!(
            section.contains("(none)"),
            "bindless instance should render (none):\n{out}"
        );
    }

    #[test]
    fn bindings_table_renders_slots_sorted_by_link_id() {
        let nodes = vec![binding_node(
            "nav",
            vec![(
                "nav-1",
                InstanceState::Running,
                vec![
                    ("sensors", vec![ProducerRef::new("core_a", "cam-1")]),
                    ("extra", vec![ProducerRef::new("core_a", "lidar-1")]),
                ],
            )],
        )];
        let out = format_stack_body(&nodes, &[], false);
        let section = bindings_section(&out);

        let sensors_at = section
            .find("sensors → cam-1@core_a")
            .expect("a slot should render its one producer's wire address");
        let extra_at = section
            .find("extra → lidar-1@core_a")
            .expect("a slot should render its one producer's wire address");
        // Sorted by link id regardless of insertion order: "extra" < "sensors".
        assert!(
            extra_at < sensors_at,
            "bindings should be sorted by link id:\n{out}"
        );
    }

    #[test]
    fn pairings_table_renders_paired_and_unpaired_rows() {
        let mut arm = node(
            "robot_arm",
            "v1",
            NodeStage::Ready,
            vec![("arm_1", InstanceState::Running)],
        );
        arm.instances[0].pairing_slots.insert(
            "controller".to_string(),
            core_node_api::SerializedPairingSlot {
                pairing_name: "arm_link".to_string(),
                pairing_tag: "v1".to_string(),
                role: "arm".to_string(),
                optional: false,
                binding: config::runtime::PairingSlotBinding::Paired {
                    peer: ProducerRef::new("core_a", "ctrl_1"),
                    peer_link_id: "arm".to_string(),
                },
            },
        );
        let mut ctrl = node(
            "arm_controller",
            "v1",
            NodeStage::Ready,
            vec![("ctrl_2", InstanceState::Running)],
        );
        ctrl.instances[0].pairing_slots.insert(
            "arm".to_string(),
            core_node_api::SerializedPairingSlot {
                pairing_name: "arm_link".to_string(),
                pairing_tag: "v1".to_string(),
                role: "controller".to_string(),
                optional: false,
                binding: config::runtime::PairingSlotBinding::Unpaired,
            },
        );
        ctrl.instances[0].pairing_slots.insert(
            "spare".to_string(),
            core_node_api::SerializedPairingSlot {
                pairing_name: "arm_link".to_string(),
                pairing_tag: "v1".to_string(),
                role: "controller".to_string(),
                optional: true,
                binding: config::runtime::PairingSlotBinding::Unpaired,
            },
        );

        let out = format_stack_body(&[arm, ctrl], &[], false);
        assert!(
            out.contains("Instance pairings"),
            "missing Instance pairings section:\n{out}"
        );
        // The paired peer carries its core_node, matching the bindings
        // table's `instance@node` producer style.
        assert!(
            out.contains("controller ⇌ ctrl_1:arm@core_a (arm_link:v1)"),
            "paired row should name the peer slot with its node:\n{out}"
        );
        assert!(
            out.contains("arm ⇌ (unpaired) [role controller of arm_link:v1]"),
            "required unpaired row should carry the role and contract:\n{out}"
        );
        assert!(
            out.contains("spare ⇌ (unpaired, optional) [role controller of arm_link:v1]"),
            "optional unpaired row should be labelled distinctly:\n{out}"
        );
    }

    #[test]
    fn pairings_section_is_omitted_when_no_instance_declares_slots() {
        let nodes = vec![node(
            "sensor",
            "v1",
            NodeStage::Ready,
            vec![("s-1", InstanceState::Running)],
        )];
        let out = format_stack_body(&nodes, &[], false);
        assert!(
            !out.contains("Instance pairings"),
            "Instance pairings section must be omitted for pairing-free stacks:\n{out}"
        );
    }

    #[test]
    fn bindings_section_renders_none_when_no_node_has_instances() {
        let nodes = vec![node("sensor", "v1", NodeStage::Added, vec![])];
        let out = format_stack_body(&nodes, &[], false);
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
    fn bindings_table_groups_multiple_nodes_with_separators_and_drops_instanceless_nodes() {
        let nodes = vec![
            binding_node(
                "alpha",
                vec![
                    (
                        "alpha-1",
                        InstanceState::Running,
                        vec![("dep", vec![ProducerRef::new("core_a", "beta-1")])],
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
        let out = format_stack_body(&nodes, &[], false);
        let section = bindings_section(&out);

        // Instance-less node never appears in the bindings table.
        assert!(
            !section.contains("ghost:v1"),
            "instance-less node should be filtered out:\n{out}"
        );

        // Each present node label appears exactly once, on its group's first
        // row, proving the node-cell continuation blanking across a real group
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
        let mut nodes = vec![binding_node(
            "机器人",
            vec![(
                "实例-uno",
                InstanceState::Running,
                vec![("传感器", vec![ProducerRef::new("core_a", "相机-1")])],
            )],
        )];
        // A pairing row mixes the `⇌` separator with CJK link and peer ids,
        // exercising the same width computation for the Pairings table.
        nodes[0].instances[0].pairing_slots.insert(
            "机械臂".to_string(),
            core_node_api::SerializedPairingSlot {
                pairing_name: "臂链".to_string(),
                pairing_tag: "v1".to_string(),
                role: "控制器".to_string(),
                optional: false,
                binding: config::runtime::PairingSlotBinding::Paired {
                    peer: ProducerRef::new("core_a", "机械臂-1"),
                    peer_link_id: "控制".to_string(),
                },
            },
        );
        let out = format_stack_body(&nodes, &[], false);

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
        // headings), the bindings table region, and the pairings table region
        // must each be internally aligned.
        let node_region = &out[..out.find("Instance bindings").expect("bindings heading")];
        assert_uniform("node table", &box_line_widths(node_region));
        assert_uniform("bindings table", &box_line_widths(bindings_section(&out)));
        assert_uniform("pairings table", &box_line_widths(pairings_section(&out)));
    }

    #[test]
    fn shorten_home_collapses_only_the_home_prefix() {
        let home = "/home/user";
        // Exact home and a child both collapse.
        assert_eq!(shorten_home_with(home, home), "~");
        assert_eq!(
            shorten_home_with("/home/user/.peppy/built_nodes/x.sif", home),
            "~/.peppy/built_nodes/x.sif"
        );
        // A sibling that merely shares the prefix text must not be rewritten.
        assert_eq!(shorten_home_with("/home/user2/x", home), "/home/user2/x");
        // Unrelated paths and a degenerate `/` home are left untouched.
        assert_eq!(shorten_home_with("/etc/passwd", home), "/etc/passwd");
        assert_eq!(shorten_home_with("/etc/passwd", "/"), "/etc/passwd");
        // A trailing slash on the resolved home is tolerated.
        assert_eq!(shorten_home_with("/home/user/x", "/home/user/"), "~/x");
    }

    /// Drops ANSI SGR escape sequences so a colored render can be compared
    /// against its plain counterpart.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                skip_csi(&mut chars);
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn colorize_is_purely_additive() {
        // Colorizing must only inject SGR codes; it must never move a column.
        // Stripping the codes back out has to reproduce the plain render byte
        // for byte, which also exercises the ANSI-aware `col_width`.
        let nodes = vec![binding_node(
            "backbone",
            vec![(
                "bk-1",
                InstanceState::Running,
                vec![("arm", vec![ProducerRef::new("core_a", "arm-1")])],
            )],
        )];
        let from = node("brain", "v1", NodeStage::Ready, vec![]);
        let to = node("sensor", "v1", NodeStage::Ready, vec![]);
        let edges = vec![SerializedEdge {
            from: from.clone(),
            to: to.clone(),
            via_contract: None,
        }];

        let plain = format_stack_body(&nodes, &edges, false);
        let colored = format_stack_body(&nodes, &edges, true);

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

        // The enclosing core-node panel performs its own width calculation,
        // including the colored node name in the header.
        let sections = [successful_section("core-a", "robot-a")];
        let plain_panel = format_stack_list(&sections, false);
        let colored_panel = format_stack_list(&sections, true);
        assert_eq!(
            strip_ansi(&colored_panel),
            plain_panel,
            "colors must not shift the outer panel border"
        );

        // The two relationships use distinct arrows so they never read alike:
        // `→` for bindings, `➔` for dependencies.
        assert!(
            bindings_section(&plain).contains("arm → arm-1@core_a"),
            "bindings should use the light arrow:\n{plain}"
        );
        assert!(
            plain.contains("brain:v1 ➔ sensor:v1"),
            "dependencies should use the heavy arrow:\n{plain}"
        );
    }
}
