use std::collections::BTreeSet;

use core_node::{IndexedNode, MatchedItem, PinStatus, SearchQuery, SearchReport, ShowOutcome};
use core_node_api::encoding::RepoItemKind;
use daemon_config::consts::PeppyDirs;

use super::search::{display_id, kind_label, matches_json, no_match_phrase, query_json, short_fingerprint};
use crate::commands::colors::{BINDING_COLOR, COUNT_COLOR, NODE_COLOR, ORANGE, RED, paint};
use crate::commands::table::render_table;
use crate::error::{Error, Result};

/// `peppy repo show <query>`: the full report of every indexed item the
/// query matches.
pub(super) fn repo_show(query: &str, json: bool) -> Result<()> {
    print!(
        "{}",
        show_rendered(
            &PeppyDirs::default(),
            query,
            json,
            crate::terminal::stdout_width()
        )?
    );
    Ok(())
}

/// The show as the command prints it, against the Peppy home it names, so
/// a test reads the text instead of capturing stdout. A query nothing
/// matches is an error, unlike a search: the command was asked for a
/// report it cannot print. `max_width` caps the human tables the way
/// `search_rendered` caps them.
pub fn show_rendered(
    peppy_dirs: &PeppyDirs,
    query: &str,
    json: bool,
    max_width: Option<usize>,
) -> Result<String> {
    let query = SearchQuery::parse(query).map_err(Error::ExecutionFailed)?;
    let outcome = core_node::show_repo_items(peppy_dirs, &query).map_err(Error::ExecutionFailed)?;
    if outcome.reports.is_empty() {
        return Err(Error::ExecutionFailed(no_match_phrase(
            &query,
            &outcome.excluded_hint,
        )));
    }
    Ok(if json {
        render_json(&query, &outcome)
    } else {
        render_human(
            &query,
            &outcome,
            crate::terminal::colors_enabled(),
            max_width,
        )
    })
}

/// The outcome as a person reads it: one report per matched identity in
/// match order, a blank line between reports. Each report names the
/// identity's published documents and, for a contract or pairing, one
/// section per way of using it, each grouped by repository the way
/// `repo list` groups nodes. Tinted with `stack list`'s palette: item
/// identities cyan, link ids yellow, counts green.
fn render_human(
    query: &SearchQuery,
    outcome: &ShowOutcome,
    colorize: bool,
    max_width: Option<usize>,
) -> String {
    let mut out = format!("{}\n", paint(colorize, NODE_COLOR, query.raw()));
    for (position, report) in outcome.reports.iter().enumerate() {
        let items: Vec<&MatchedItem> = outcome
            .matches
            .iter()
            .filter(|m| m.name == report.name && m.tag == report.tag)
            .collect();
        if position > 0 {
            out.push('\n');
        }
        out.push_str(&report_block(
            report,
            &items,
            &outcome.excluded_hint,
            colorize,
            max_width,
        ));
    }
    out
}

/// One identity's report: one published line per document of the
/// identity, then the four usage sections. A contract or pairing nobody
/// uses says so; the other kinds have no usage sections to be empty, so
/// their published line is the whole answer.
fn report_block(
    report: &SearchReport,
    items: &[&MatchedItem],
    excluded_hint: &str,
    colorize: bool,
    max_width: Option<usize>,
) -> String {
    let mut out = String::new();
    for item in items {
        out.push_str(&published_line(item));
    }

    let mut sections = String::new();
    const CONTRACT_HEADERS: [&str; 5] = ["NODE", "TAG", "SLOT", "PIN", "PATH"];
    const PAIRING_HEADERS: [&str; 6] = ["NODE", "TAG", "ROLE", "SLOT", "PIN", "PATH"];
    sections.push_str(&section(
        "Implemented by",
        &CONTRACT_HEADERS,
        &report.implementers,
        |hit| &hit.node,
        |hit| {
            vec![
                slot_cell(&hit.link_id, None, colorize),
                pin_cell(hit.sha256.as_deref(), &hit.pin, colorize),
            ]
        },
        colorize,
        max_width,
    ));
    sections.push_str(&section(
        "Consumed by",
        &CONTRACT_HEADERS,
        &report.consumers,
        |hit| &hit.node,
        |hit| {
            vec![
                slot_cell(&hit.link_id, Some(hit.cardinality.as_str()), colorize),
                pin_cell(hit.sha256.as_deref(), &hit.pin, colorize),
            ]
        },
        colorize,
        max_width,
    ));
    sections.push_str(&section(
        "Pairing roles played by",
        &PAIRING_HEADERS,
        &report.participants,
        |hit| &hit.node,
        |hit| {
            vec![
                hit.role.clone(),
                slot_cell(&hit.link_id, hit.optional.then_some("optional"), colorize),
                pin_cell(hit.sha256.as_deref(), &hit.pin, colorize),
            ]
        },
        colorize,
        max_width,
    ));
    sections.push_str(&section(
        "Observed by",
        &PAIRING_HEADERS,
        &report.observers,
        |hit| &hit.node,
        |hit| {
            vec![
                hit.role.clone(),
                slot_cell(&hit.link_id, Some(hit.cardinality.as_str()), colorize),
                pin_cell(hit.sha256.as_deref(), &hit.pin, colorize),
            ]
        },
        colorize,
        max_width,
    ));
    let usable_by_nodes = items
        .iter()
        .any(|item| matches!(item.kind, RepoItemKind::Contract | RepoItemKind::Pairing));
    if sections.is_empty() && usable_by_nodes {
        out.push_str(&format!(
            "\nNo indexed node implements, consumes, participates in, or observes `{}:{}`{}\n",
            report.name, report.tag, excluded_hint
        ));
    }
    out.push_str(&sections);
    out
}

/// Where one matching document is stored: the copy a plain reference
/// resolves to, or the one carrying the queried digest.
fn published_line(item: &MatchedItem) -> String {
    format!(
        "  {} {} published by {} at {} (sha256 {})\n",
        kind_label(item.kind),
        display_id(item),
        item.published.repo_label,
        item.published.path,
        item.published.sha256
    )
}

/// The SLOT column: the `link_id`, tinted like every link id, with the
/// cardinality or `optional` qualifier the manifest declares on it.
fn slot_cell(link_id: &str, qualifier: Option<&str>, colorize: bool) -> String {
    let link = paint(colorize, BINDING_COLOR, link_id);
    match qualifier {
        Some(qualifier) => format!("{link} ({qualifier})"),
        None => link,
    }
}

/// What the claim's pin does at sync, in red when a sync would refuse it.
fn pin_cell(sha256: Option<&str>, pin: &PinStatus, colorize: bool) -> String {
    let short = short_fingerprint(sha256.unwrap_or_default());
    match pin {
        PinStatus::Unpinned => "unpinned".to_owned(),
        PinStatus::Current => format!("pin {short} (current)"),
        PinStatus::Resolvable { repo_label, .. } => {
            format!("pin {short} (cached copy in {repo_label})")
        }
        PinStatus::Unresolvable => paint(colorize, RED, &format!("pin {short} (not in cache)")),
        PinStatus::Unusable { reason } => paint(colorize, RED, &format!("unusable pin ({reason})")),
    }
}

/// The indent every table line carries under its repository label.
const INDENT: &str = "    ";

/// One section: a title counting the distinct nodes, then the hits grouped
/// by repository in the order the report lists them (repository priority),
/// each group a table under `headers`, one row per hit
/// (`name tag <cells> path`). A shadowed node's note rides its PATH cell as
/// an extra line, so it stays inside the outline and wraps with the table.
fn section<T>(
    title: &str,
    headers: &[&str],
    hits: &[T],
    node_of: impl Fn(&T) -> &IndexedNode,
    cells_of: impl Fn(&T) -> Vec<String>,
    colorize: bool,
    max_width: Option<usize>,
) -> String {
    if hits.is_empty() {
        return String::new();
    }
    let nodes: BTreeSet<(u32, &str, &str)> = hits
        .iter()
        .map(|hit| {
            let node = node_of(hit);
            (
                node.repo_id,
                node.node_name.as_str(),
                node.node_tag.as_str(),
            )
        })
        .collect();
    let mut out = format!(
        "\n{title} {} indexed node{}\n",
        paint(colorize, COUNT_COLOR, &nodes.len().to_string()),
        if nodes.len() == 1 { "" } else { "s" }
    );

    let mut start = 0;
    while start < hits.len() {
        let repo_id = node_of(&hits[start]).repo_id;
        let end = hits[start..]
            .iter()
            .position(|hit| node_of(hit).repo_id != repo_id)
            .map_or(hits.len(), |offset| start + offset);
        let group = &hits[start..end];
        out.push_str(&format!("  {}:\n", node_of(&group[0]).repo_label));
        let rows: Vec<Vec<String>> = group
            .iter()
            .map(|hit| {
                let node = node_of(hit);
                let mut cells = vec![
                    paint(colorize, NODE_COLOR, &node.node_name),
                    paint(colorize, NODE_COLOR, &node.node_tag),
                ];
                cells.extend(cells_of(hit));
                cells.push(match &node.shadowed_by {
                    Some(by) => format!(
                        "{}\n{}",
                        node.path,
                        paint(colorize, ORANGE, &format!("(shadowed by {by})"))
                    ),
                    None => node.path.clone(),
                });
                cells
            })
            .collect();
        let mut table = String::new();
        render_table(
            &mut table,
            headers,
            &[rows],
            max_width.map(|width| width.saturating_sub(INDENT.len())),
        );
        out.push_str(&indented(&table));
        start = end;
    }
    out
}

/// The rendered table placed under its repository label: every line
/// indented, the trailing blank line dropped (sections space themselves).
fn indented(table: &str) -> String {
    table
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| format!("{INDENT}{line}\n"))
        .collect()
}

/// Machine-readable output: the parsed query, every match with its full
/// fingerprint, and one usage report per matched identity under
/// `reports`, in match order.
fn render_json(query: &SearchQuery, outcome: &ShowOutcome) -> String {
    let doc = serde_json::json!({
        "query": query_json(query),
        "matches": matches_json(&outcome.matches),
        "reports": outcome.reports.iter().map(report_json).collect::<Vec<_>>(),
        "excluded": outcome.excluded_hint,
    });
    format!("{doc}\n")
}

/// One usage report: every section with each hit's node, slot facts, raw
/// pin and pin state.
fn report_json(report: &SearchReport) -> serde_json::Value {
    let implementers: Vec<serde_json::Value> = report
        .implementers
        .iter()
        .map(|hit| {
            serde_json::json!({
                "node": node_json(&hit.node),
                "link_id": hit.link_id,
                "sha256": hit.sha256,
                "pin": pin_json(&hit.pin),
            })
        })
        .collect();
    let consumers: Vec<serde_json::Value> = report
        .consumers
        .iter()
        .map(|hit| {
            serde_json::json!({
                "node": node_json(&hit.node),
                "link_id": hit.link_id,
                "cardinality": hit.cardinality,
                "sha256": hit.sha256,
                "pin": pin_json(&hit.pin),
            })
        })
        .collect();
    let participants: Vec<serde_json::Value> = report
        .participants
        .iter()
        .map(|hit| {
            serde_json::json!({
                "node": node_json(&hit.node),
                "role": hit.role,
                "link_id": hit.link_id,
                "optional": hit.optional,
                "sha256": hit.sha256,
                "pin": pin_json(&hit.pin),
            })
        })
        .collect();
    let observers: Vec<serde_json::Value> = report
        .observers
        .iter()
        .map(|hit| {
            serde_json::json!({
                "node": node_json(&hit.node),
                "role": hit.role,
                "link_id": hit.link_id,
                "cardinality": hit.cardinality,
                "sha256": hit.sha256,
                "pin": pin_json(&hit.pin),
            })
        })
        .collect();
    serde_json::json!({
        "name": report.name,
        "tag": report.tag,
        "implementers": implementers,
        "consumers": consumers,
        "participants": participants,
        "observers": observers,
    })
}

fn node_json(node: &IndexedNode) -> serde_json::Value {
    serde_json::json!({
        "node_name": node.node_name,
        "node_tag": node.node_tag,
        "repo_id": node.repo_id,
        "repo_label": node.repo_label,
        "source_type": node.source_type.as_str(),
        "path": node.path,
        "shadowed_by": node.shadowed_by,
    })
}

fn pin_json(pin: &PinStatus) -> serde_json::Value {
    match pin {
        PinStatus::Unpinned => serde_json::json!({ "status": "unpinned" }),
        PinStatus::Current => serde_json::json!({ "status": "current" }),
        PinStatus::Resolvable {
            repo_id,
            repo_label,
            path,
        } => serde_json::json!({
            "status": "resolvable",
            "repo_id": repo_id,
            "repo_label": repo_label,
            "path": path,
        }),
        PinStatus::Unresolvable => serde_json::json!({ "status": "unresolvable" }),
        PinStatus::Unusable { reason } => {
            serde_json::json!({ "status": "unusable", "reason": reason })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::search::fixtures::*;
    use super::*;
    use config::node::Cardinality;
    use core_node::{Consumer, Implementer, Observer, Participant, PublishedDoc};
    use daemon_config::repository::ManifestFingerprint;

    fn shown(reports: Vec<SearchReport>, matches: Vec<MatchedItem>) -> ShowOutcome {
        ShowOutcome {
            matches,
            reports,
            excluded_hint: String::new(),
        }
    }

    fn full() -> ShowOutcome {
        let report = SearchReport {
            implementers: vec![
                Implementer {
                    node: node("uvc_camera_linux", "uvc_camera/linux/peppy.json5"),
                    link_id: "camera".to_owned(),
                    sha256: Some(sha('a')),
                    pin: PinStatus::Current,
                },
                Implementer {
                    node: node("realsense_d4xx", "realsense_d4xx/peppy.json5"),
                    link_id: "camera".to_owned(),
                    sha256: None,
                    pin: PinStatus::Unpinned,
                },
            ],
            consumers: vec![Consumer {
                node: node(
                    "episode_recorder",
                    "example_robot/episode_recorder/peppy.json5",
                ),
                link_id: "camera".to_owned(),
                cardinality: Cardinality::OneOrMore,
                sha256: None,
                pin: PinStatus::Unpinned,
            }],
            participants: vec![Participant {
                node: node("viewer", "viewer/peppy.json5"),
                role: "viewer".to_owned(),
                link_id: "camera".to_owned(),
                optional: true,
                sha256: None,
                pin: PinStatus::Unpinned,
            }],
            observers: vec![Observer {
                node: IndexedNode {
                    shadowed_by: Some("/home/user/workspace".to_owned()),
                    ..node("dashboard", "dashboard/peppy.json5")
                },
                role: "camera".to_owned(),
                link_id: "watch".to_owned(),
                cardinality: Cardinality::ZeroOrOne,
                sha256: Some(sha('c')),
                pin: PinStatus::Resolvable {
                    repo_id: 7,
                    repo_label: "/home/user/mirror".to_owned(),
                    path: "/home/user/mirror/rgb.json5".to_owned(),
                },
            }],
            ..empty_report()
        };
        shown(
            vec![report],
            vec![matched(
                RepoItemKind::Contract,
                "rgb_camera",
                "v1",
                contract(),
            )],
        )
    }

    /// Every section, with the published document first, rows aligned
    /// within a repository, the slot facts spelled as the manifest spells
    /// them, and the shadowing note where a lower-id repository wins.
    #[test]
    fn human_output_lists_every_section_with_aligned_columns() {
        let text = render_human(&query("rgb_camera:v1"), &full(), false, None);

        let expected = [
            "rgb_camera:v1",
            "  contract rgb_camera:v1 published by https://github.com/Peppy-bot/contracts-hub.git (ref: main) at cameras/rgb_camera.json5 (sha256 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)",
            "",
            "Implemented by 2 indexed nodes",
            "  https://github.com/Peppy-bot/nodes-hub.git (ref: main):",
            "    ┌──────────────────┬─────┬────────┬────────────────────────┬──────────────────────────────┐",
            "    │ NODE             │ TAG │ SLOT   │ PIN                    │ PATH                         │",
            "    ├──────────────────┼─────┼────────┼────────────────────────┼──────────────────────────────┤",
            "    │ uvc_camera_linux │ v1  │ camera │ pin aaaaaaaa (current) │ uvc_camera/linux/peppy.json5 │",
            "    │ realsense_d4xx   │ v1  │ camera │ unpinned               │ realsense_d4xx/peppy.json5   │",
            "    └──────────────────┴─────┴────────┴────────────────────────┴──────────────────────────────┘",
            "",
            "Consumed by 1 indexed node",
            "  https://github.com/Peppy-bot/nodes-hub.git (ref: main):",
            "    ┌──────────────────┬─────┬──────────────────────┬──────────┬────────────────────────────────────────────┐",
            "    │ NODE             │ TAG │ SLOT                 │ PIN      │ PATH                                       │",
            "    ├──────────────────┼─────┼──────────────────────┼──────────┼────────────────────────────────────────────┤",
            "    │ episode_recorder │ v1  │ camera (one_or_more) │ unpinned │ example_robot/episode_recorder/peppy.json5 │",
            "    └──────────────────┴─────┴──────────────────────┴──────────┴────────────────────────────────────────────┘",
            "",
            "Pairing roles played by 1 indexed node",
            "  https://github.com/Peppy-bot/nodes-hub.git (ref: main):",
            "    ┌────────┬─────┬────────┬───────────────────┬──────────┬────────────────────┐",
            "    │ NODE   │ TAG │ ROLE   │ SLOT              │ PIN      │ PATH               │",
            "    ├────────┼─────┼────────┼───────────────────┼──────────┼────────────────────┤",
            "    │ viewer │ v1  │ viewer │ camera (optional) │ unpinned │ viewer/peppy.json5 │",
            "    └────────┴─────┴────────┴───────────────────┴──────────┴────────────────────┘",
            "",
            "Observed by 1 indexed node",
            "  https://github.com/Peppy-bot/nodes-hub.git (ref: main):",
            "    ┌───────────┬─────┬────────┬─────────────────────┬─────────────────────────────────────────────────┬────────────────────────────────────┐",
            "    │ NODE      │ TAG │ ROLE   │ SLOT                │ PIN                                             │ PATH                               │",
            "    ├───────────┼─────┼────────┼─────────────────────┼─────────────────────────────────────────────────┼────────────────────────────────────┤",
            "    │ dashboard │ v1  │ camera │ watch (zero_or_one) │ pin cccccccc (cached copy in /home/user/mirror) │ dashboard/peppy.json5              │",
            "    │           │     │        │                     │                                                 │ (shadowed by /home/user/workspace) │",
            "    └───────────┴─────┴────────┴─────────────────────┴─────────────────────────────────────────────────┴────────────────────────────────────┘",
            "",
        ]
        .join("\n");
        assert_eq!(text, expected);
    }

    /// Under a width limit the widest columns give way first, ties
    /// rightmost, and over-long cells wrap onto continuation lines inside
    /// the outline, so no outline line exceeds the limit.
    #[test]
    fn human_output_wraps_wide_tables_to_the_width_limit() {
        let text = render_human(&query("rgb_camera:v1"), &full(), false, Some(80));

        for line in text.lines() {
            if ['│', '┐', '┤', '┘'].iter().any(|c| line.ends_with(*c)) {
                assert!(line.chars().count() <= 80, "{line}\n{text}");
            }
        }
        let expected = [
            "rgb_camera:v1",
            "  contract rgb_camera:v1 published by https://github.com/Peppy-bot/contracts-hub.git (ref: main) at cameras/rgb_camera.json5 (sha256 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)",
            "",
            "Implemented by 2 indexed nodes",
            "  https://github.com/Peppy-bot/nodes-hub.git (ref: main):",
            "    ┌──────────────────┬─────┬────────┬────────────────────┬───────────────────┐",
            "    │ NODE             │ TAG │ SLOT   │ PIN                │ PATH              │",
            "    ├──────────────────┼─────┼────────┼────────────────────┼───────────────────┤",
            "    │ uvc_camera_linux │ v1  │ camera │ pin aaaaaaaa       │ uvc_camera/linux/ │",
            "    │                  │     │        │ (current)          │ peppy.json5       │",
            "    │ realsense_d4xx   │ v1  │ camera │ unpinned           │ realsense_d4xx/pe │",
            "    │                  │     │        │                    │ ppy.json5         │",
            "    └──────────────────┴─────┴────────┴────────────────────┴───────────────────┘",
            "",
            "Consumed by 1 indexed node",
            "  https://github.com/Peppy-bot/nodes-hub.git (ref: main):",
            "    ┌──────────────────┬─────┬───────────────────┬──────────┬──────────────────┐",
            "    │ NODE             │ TAG │ SLOT              │ PIN      │ PATH             │",
            "    ├──────────────────┼─────┼───────────────────┼──────────┼──────────────────┤",
            "    │ episode_recorder │ v1  │ camera            │ unpinned │ example_robot/ep │",
            "    │                  │     │ (one_or_more)     │          │ isode_recorder/p │",
            "    │                  │     │                   │          │ eppy.json5       │",
            "    └──────────────────┴─────┴───────────────────┴──────────┴──────────────────┘",
            "",
            "Pairing roles played by 1 indexed node",
            "  https://github.com/Peppy-bot/nodes-hub.git (ref: main):",
            "    ┌────────┬─────┬────────┬───────────────────┬──────────┬───────────────────┐",
            "    │ NODE   │ TAG │ ROLE   │ SLOT              │ PIN      │ PATH              │",
            "    ├────────┼─────┼────────┼───────────────────┼──────────┼───────────────────┤",
            "    │ viewer │ v1  │ viewer │ camera (optional) │ unpinned │ viewer/peppy.json │",
            "    │        │     │        │                   │          │ 5                 │",
            "    └────────┴─────┴────────┴───────────────────┴──────────┴───────────────────┘",
            "",
            "Observed by 1 indexed node",
            "  https://github.com/Peppy-bot/nodes-hub.git (ref: main):",
            "    ┌───────────┬─────┬────────┬───────────────┬───────────────┬───────────────┐",
            "    │ NODE      │ TAG │ ROLE   │ SLOT          │ PIN           │ PATH          │",
            "    ├───────────┼─────┼────────┼───────────────┼───────────────┼───────────────┤",
            "    │ dashboard │ v1  │ camera │ watch         │ pin cccccccc  │ dashboard/pep │",
            "    │           │     │        │ (zero_or_one) │ (cached copy  │ py.json5      │",
            "    │           │     │        │               │ in            │ (shadowed by  │",
            "    │           │     │        │               │ /home/user/mi │ /home/user/wo │",
            "    │           │     │        │               │ rror)         │ rkspace)      │",
            "    └───────────┴─────┴────────┴───────────────┴───────────────┴───────────────┘",
            "",
        ]
        .join("\n");
        assert_eq!(text, expected, "\n{text}");
    }

    /// A limit narrower than the headers still yields a coherent table:
    /// no column shrinks below its header, the outline holds at that
    /// floor, and every over-long cell wraps.
    #[test]
    fn human_output_never_shrinks_a_column_below_its_header() {
        let text = render_human(&query("rgb_camera:v1"), &full(), false, Some(10));

        assert!(
            text.contains("│ NODE │ TAG │ SLOT │ PIN │ PATH │"),
            "{text}"
        );
        assert!(text.contains("│ uvc_ │ v1  │ came │"), "{text}");
    }

    /// The query and each node's name and tag are cyan, link ids yellow,
    /// and section counts green, exactly as `stack list` tints them;
    /// padding is computed on the plain text so colour never skews the
    /// outline.
    #[test]
    fn human_output_tints_with_the_stack_list_palette() {
        use crate::commands::colors::RESET;

        let coloured = render_human(&query("rgb_camera:v1"), &full(), true, None);

        assert!(
            coloured.starts_with(&format!("{NODE_COLOR}rgb_camera:v1{RESET}\n")),
            "{coloured}"
        );
        assert!(
            coloured.contains(&format!("│ {NODE_COLOR}uvc_camera_linux{RESET} ")),
            "{coloured}"
        );
        assert!(
            coloured.contains(&format!("{NODE_COLOR}v1{RESET}")),
            "{coloured}"
        );
        assert!(
            coloured.contains(&format!("{BINDING_COLOR}camera{RESET}")),
            "{coloured}"
        );
        assert!(
            coloured.contains(&format!(
                "Implemented by {COUNT_COLOR}2{RESET} indexed nodes"
            )),
            "{coloured}"
        );
    }

    /// The two pin states a sync refuses are red: the column is padded on
    /// the plain text so colour never skews it.
    #[test]
    fn human_output_paints_the_pins_a_sync_would_refuse() {
        let report = SearchReport {
            implementers: vec![
                Implementer {
                    node: node("stale", "stale/peppy.json5"),
                    link_id: "camera".to_owned(),
                    sha256: Some(sha('b')),
                    pin: PinStatus::Unresolvable,
                },
                Implementer {
                    node: node("typo", "typo/peppy.json5"),
                    link_id: "camera".to_owned(),
                    sha256: Some("abc".to_owned()),
                    pin: PinStatus::Unusable {
                        reason: "fingerprint is not 64 hexadecimal characters: abc".to_owned(),
                    },
                },
            ],
            ..empty_report()
        };
        let outcome = shown(
            vec![report],
            vec![matched(
                RepoItemKind::Contract,
                "rgb_camera",
                "v1",
                contract(),
            )],
        );

        let plain = render_human(&query("rgb_camera:v1"), &outcome, false, None);
        assert!(
            plain.contains("    │ stale │ v1  │ camera │ pin bbbbbbbb (not in cache)"),
            "{plain}"
        );
        assert!(
            plain.contains(
                "    │ typo  │ v1  │ camera │ unusable pin (fingerprint is not 64 hexadecimal characters: abc) │ typo/peppy.json5"
            ),
            "{plain}"
        );
        assert!(!plain.contains('\x1b'), "no colour without a terminal");

        let coloured = render_human(&query("rgb_camera:v1"), &outcome, true, None);
        assert!(
            coloured.contains(&format!("{RED}pin bbbbbbbb (not in cache)")),
            "{coloured}"
        );
        assert!(
            coloured.contains(&format!("{RED}unusable pin (fingerprint")),
            "{coloured}"
        );
        assert!(
            !coloured.contains(&format!("{RED}stale")),
            "a healthy cell is never red: {coloured}"
        );

        let narrow = render_human(&query("rgb_camera:v1"), &outcome, true, Some(60));
        assert!(
            narrow.matches(RED).count() > coloured.matches(RED).count(),
            "every piece of a wrapped red pin is painted: {narrow}"
        );
    }

    /// Both namespaces are reported when the identity is published as a
    /// contract and as a pairing, and a pair nobody uses says so once.
    #[test]
    fn human_output_names_both_published_documents() {
        let pairing = PublishedDoc {
            repo_id: 1002,
            repo_label: CONTRACTS.to_owned(),
            path: "pairings/rgb_camera.json5".to_owned(),
            sha256: ManifestFingerprint::parse(&sha('d')).unwrap(),
        };
        let outcome = shown(
            vec![empty_report()],
            vec![
                matched(RepoItemKind::Contract, "rgb_camera", "v1", contract()),
                matched(RepoItemKind::Pairing, "rgb_camera", "v1", pairing),
            ],
        );

        let text = render_human(&query("rgb_camera:v1"), &outcome, false, None);

        assert!(
            text.contains("  contract rgb_camera:v1 published by https://github.com/Peppy-bot/contracts-hub.git (ref: main) at cameras/rgb_camera.json5 (sha256 aaaa"),
            "{text}"
        );
        assert!(
            text.contains("  pairing rgb_camera:v1 published by https://github.com/Peppy-bot/contracts-hub.git (ref: main) at pairings/rgb_camera.json5 (sha256 dddd"),
            "{text}"
        );
        assert!(
            text.contains(
                "No indexed node implements, consumes, participates in, or observes `rgb_camera:v1`"
            ),
            "{text}"
        );
        assert!(!text.contains("nothing in any configured"), "{text}");
    }

    /// A launcher has no usage sections to be empty, so its published
    /// line is the whole answer: no "No indexed node ..." line.
    #[test]
    fn human_output_details_a_single_launcher_match() {
        let outcome = shown(
            vec![SearchReport {
                name: "openarm_boot".to_owned(),
                tag: String::new(),
                ..empty_report()
            }],
            vec![matched(
                RepoItemKind::Launcher,
                "openarm_boot",
                "",
                PublishedDoc {
                    repo_id: 1001,
                    repo_label: LAUNCHERS.to_owned(),
                    path: "openarm_boot.json5".to_owned(),
                    sha256: ManifestFingerprint::parse(&sha('e')).unwrap(),
                },
            )],
        );

        let text = render_human(&query("openarm_boot"), &outcome, false, None);

        assert_eq!(
            text,
            format!(
                "openarm_boot\n\
                 \x20 launcher openarm_boot published by {LAUNCHERS} at openarm_boot.json5 (sha256 {})\n",
                sha('e')
            )
        );
    }

    /// One report block per matched identity in match order, a blank line
    /// between blocks, and no match table.
    #[test]
    fn human_output_prints_a_block_per_matched_identity() {
        let mut second = empty_report();
        second.name = "sim_rgb_camera_link".to_owned();
        let mut pairing = matched(
            RepoItemKind::Pairing,
            "sim_rgb_camera_link",
            "v1",
            PublishedDoc {
                repo_id: 1002,
                repo_label: CONTRACTS.to_owned(),
                path: "cameras/sim_rgb_camera_link.json5".to_owned(),
                sha256: ManifestFingerprint::parse(&sha('d')).unwrap(),
            },
        );
        pairing.exact = false;
        let outcome = shown(
            vec![empty_report(), second],
            vec![
                matched(RepoItemKind::Contract, "rgb_camera", "v1", contract()),
                pairing,
            ],
        );

        let text = render_human(&query("rgb_camera"), &outcome, false, None);

        assert_eq!(
            text,
            format!(
                "rgb_camera\n\
                 \x20 contract rgb_camera:v1 published by {CONTRACTS} at \
                 cameras/rgb_camera.json5 (sha256 {a})\n\
                 \nNo indexed node implements, consumes, participates in, or observes \
                 `rgb_camera:v1`\n\
                 \n\
                 \x20 pairing sim_rgb_camera_link:v1 published by {CONTRACTS} at \
                 cameras/sim_rgb_camera_link.json5 (sha256 {d})\n\
                 \nNo indexed node implements, consumes, participates in, or observes \
                 `sim_rgb_camera_link:v1`\n",
                a = sha('a'),
                d = sha('d'),
            ),
            "\n{text}"
        );
    }

    /// The JSON carries the parsed query, every match with its full
    /// fingerprint, and one report per matched identity under `reports`,
    /// with `null` where a value is absent.
    #[test]
    fn json_output_carries_matches_and_reports() {
        let text = render_json(&query("rgb_camera:v1"), &full());

        assert!(text.ends_with('\n'));
        let doc: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(
            doc["query"],
            serde_json::json!({
                "raw": "rgb_camera:v1",
                "name": "rgb_camera",
                "tag": "v1",
                "sha256": serde_json::Value::Null,
            })
        );
        let matches = doc["matches"].as_array().expect("array");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["kind"], "contract");
        assert_eq!(matches[0]["name"], "rgb_camera");
        assert_eq!(matches[0]["tag"], "v1");
        assert_eq!(matches[0]["exact"], true);
        assert_eq!(matches[0]["published"]["repo_id"], 1002);
        assert_eq!(matches[0]["published"]["sha256"], sha('a'));
        assert_eq!(doc["excluded"], "");

        let reports = doc["reports"].as_array().expect("array");
        assert_eq!(reports.len(), 1, "one report per matched identity");
        let report = &reports[0];
        assert_eq!(report["name"], "rgb_camera");
        assert_eq!(report["tag"], "v1");
        let implementers = report["implementers"].as_array().expect("array");
        assert_eq!(implementers.len(), 2);
        assert_eq!(implementers[0]["node"]["node_name"], "uvc_camera_linux");
        assert_eq!(implementers[0]["node"]["source_type"], "git");
        assert_eq!(implementers[0]["node"]["repo_id"], 1000);
        assert!(implementers[0]["node"]["shadowed_by"].is_null());
        assert_eq!(implementers[0]["link_id"], "camera");
        assert_eq!(implementers[0]["sha256"], sha('a'));
        assert_eq!(
            implementers[0]["pin"],
            serde_json::json!({ "status": "current" })
        );
        assert!(implementers[1]["sha256"].is_null());
        assert_eq!(implementers[1]["pin"]["status"], "unpinned");

        assert_eq!(report["consumers"][0]["cardinality"], "one_or_more");
        assert_eq!(report["participants"][0]["role"], "viewer");
        assert_eq!(report["participants"][0]["optional"], true);

        let observer = &report["observers"][0];
        assert_eq!(observer["cardinality"], "zero_or_one");
        assert_eq!(observer["node"]["shadowed_by"], "/home/user/workspace");
        assert_eq!(
            observer["pin"],
            serde_json::json!({
                "status": "resolvable",
                "repo_id": 7,
                "repo_label": "/home/user/mirror",
                "path": "/home/user/mirror/rgb.json5",
            })
        );

        let digest = sha('a');
        let text = render_json(&query(&format!("rgb_camera:v1@{digest}")), &full());
        let doc: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(doc["query"]["sha256"], digest);
    }

    /// An unusable pin's JSON carries the reason a sync would print.
    #[test]
    fn json_output_explains_an_unusable_pin() {
        let value = pin_json(&PinStatus::Unusable {
            reason: "fingerprint is empty".to_owned(),
        });
        assert_eq!(
            value,
            serde_json::json!({ "status": "unusable", "reason": "fingerprint is empty" })
        );
        assert_eq!(
            pin_json(&PinStatus::Unresolvable),
            serde_json::json!({ "status": "unresolvable" })
        );
    }
}
