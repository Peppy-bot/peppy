use std::collections::BTreeSet;

use core_node::{IndexedNode, PinStatus, PublishedDoc, SearchReport};
use daemon_config::consts::PeppyDirs;
use daemon_config::source::ItemRef;

use super::{ORANGE, RED, paint};
use crate::error::{Error, Result};

/// `peppy repo search <name>:<tag>`: who uses a contract or pairing.
pub(super) fn repo_search(identity: &str, json: bool) -> Result<()> {
    print!(
        "{}",
        search_rendered(&PeppyDirs::default(), identity, json)?
    );
    Ok(())
}

/// The search as the command prints it, against the Peppy home it names,
/// so a test reads the text instead of capturing stdout.
pub fn search_rendered(peppy_dirs: &PeppyDirs, identity: &str, json: bool) -> Result<String> {
    let reference = ItemRef::parse(identity, "search").map_err(Error::ExecutionFailed)?;
    let report = core_node::search_identity(peppy_dirs, &reference.name, &reference.tag)
        .map_err(Error::ExecutionFailed)?;
    Ok(if json {
        render_json(&reference, &report)
    } else {
        render_human(&reference, &report, crate::terminal::colors_enabled())
    })
}

/// The report as a person reads it: the published documents first, then
/// one section per way of using the identity, each grouped by repository
/// the way `repo list` groups nodes. A section with no hits is left out;
/// a report with none says so.
fn render_human(reference: &ItemRef, report: &SearchReport, colorize: bool) -> String {
    let mut out = format!("{reference}\n");
    let published: Vec<String> = [("contract", &report.contract), ("pairing", &report.pairing)]
        .into_iter()
        .filter_map(|(kind, doc)| doc.as_ref().map(|doc| published_line(kind, doc)))
        .collect();
    if published.is_empty() {
        out.push_str(&format!(
            "  no configured repository publishes a contract or pairing named `{reference}`; \
             check the name, or register its repository (`peppy repo add`) and run \
             `peppy repo refresh`\n"
        ));
    }
    for line in published {
        out.push_str(&line);
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
                slot_cell(&hit.link_id, None),
                pin_cell(hit.sha256.as_deref(), &hit.pin),
            ]
        },
        colorize,
    ));
    sections.push_str(&section(
        "Consumed by",
        &CONTRACT_HEADERS,
        &report.consumers,
        |hit| &hit.node,
        |hit| {
            vec![
                slot_cell(&hit.link_id, Some(hit.cardinality.as_str())),
                pin_cell(hit.sha256.as_deref(), &hit.pin),
            ]
        },
        colorize,
    ));
    sections.push_str(&section(
        "Pairing roles played by",
        &PAIRING_HEADERS,
        &report.participants,
        |hit| &hit.node,
        |hit| {
            vec![
                Cell::plain(&hit.role),
                slot_cell(&hit.link_id, hit.optional.then_some("optional")),
                pin_cell(hit.sha256.as_deref(), &hit.pin),
            ]
        },
        colorize,
    ));
    sections.push_str(&section(
        "Observed by",
        &PAIRING_HEADERS,
        &report.observers,
        |hit| &hit.node,
        |hit| {
            vec![
                Cell::plain(&hit.role),
                slot_cell(&hit.link_id, Some(hit.cardinality.as_str())),
                pin_cell(hit.sha256.as_deref(), &hit.pin),
            ]
        },
        colorize,
    ));
    if sections.is_empty() {
        out.push_str(&format!(
            "\nNo indexed node implements, consumes, participates in, or observes `{reference}`{}\n",
            report.excluded_hint
        ));
    }
    out.push_str(&sections);
    out
}

fn published_line(kind: &str, doc: &PublishedDoc) -> String {
    format!(
        "  {kind} published by {} at {} (sha256 {})\n",
        doc.repo_label, doc.path, doc.sha256
    )
}

/// One column of a row, painted when it carries a state worth the colour.
struct Cell {
    text: String,
    colour: Option<&'static str>,
}

impl Cell {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            colour: None,
        }
    }
}

/// The SLOT column: the `link_id`, with the cardinality or `optional`
/// qualifier the manifest declares on it.
fn slot_cell(link_id: &str, qualifier: Option<&str>) -> Cell {
    Cell::plain(match qualifier {
        Some(qualifier) => format!("{link_id} ({qualifier})"),
        None => link_id.to_owned(),
    })
}

/// What the claim's pin does at sync, in red when a sync would refuse it.
fn pin_cell(sha256: Option<&str>, pin: &PinStatus) -> Cell {
    let short: String = sha256.unwrap_or_default().chars().take(8).collect();
    match pin {
        PinStatus::Unpinned => Cell::plain("unpinned"),
        PinStatus::Current => Cell::plain(format!("pin {short} (current)")),
        PinStatus::Resolvable { repo_label, .. } => {
            Cell::plain(format!("pin {short} (cached copy in {repo_label})"))
        }
        PinStatus::Unresolvable => Cell {
            text: format!("pin {short} (not in cache)"),
            colour: Some(RED),
        },
        PinStatus::Unusable { reason } => Cell {
            text: format!("unusable pin ({reason})"),
            colour: Some(RED),
        },
    }
}

/// One section: a title counting the distinct nodes, then the hits grouped
/// by repository in the order the report lists them (repository priority),
/// each group a table under `headers`, one row per hit
/// (`name tag <cells> path`), with the shadowing note appended when a
/// lower-id repository provides the same identity.
fn section<T>(
    title: &str,
    headers: &[&str],
    hits: &[T],
    node_of: impl Fn(&T) -> &IndexedNode,
    cells_of: impl Fn(&T) -> Vec<Cell>,
    colorize: bool,
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
        nodes.len(),
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
        let rows: Vec<(Vec<Cell>, String)> = group
            .iter()
            .map(|hit| {
                let node = node_of(hit);
                let mut cells = vec![Cell::plain(&node.node_name), Cell::plain(&node.node_tag)];
                cells.extend(cells_of(hit));
                cells.push(Cell::plain(&node.path));
                let suffix = node
                    .shadowed_by
                    .as_deref()
                    .map(|by| paint(&format!("  (shadowed by {by})"), ORANGE, colorize))
                    .unwrap_or_default();
                (cells, suffix)
            })
            .collect();
        out.push_str(&table(headers, &rows, colorize));
        start = end;
    }
    out
}

/// A header row, then the data rows, every column but the last padded to
/// the group's widest value, headers included. Padding is measured on the
/// plain text and applied before painting, so colour never skews a column.
fn table(headers: &[&str], rows: &[(Vec<Cell>, String)], colorize: bool) -> String {
    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(column, header)| {
            rows.iter()
                .map(|(cells, _)| cells[column].text.len())
                .chain([header.len()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let header_cells: Vec<Cell> = headers.iter().copied().map(Cell::plain).collect();
    let mut out = rendered_row(&header_cells, "", &widths, colorize);
    for (cells, suffix) in rows {
        out.push_str(&rendered_row(cells, suffix, &widths, colorize));
    }
    out
}

/// One table line under the section's repository label.
fn rendered_row(cells: &[Cell], suffix: &str, widths: &[usize], colorize: bool) -> String {
    let mut out = String::from("    ");
    for (column, cell) in cells.iter().enumerate() {
        let last = column + 1 == cells.len();
        let text = if last {
            cell.text.clone()
        } else {
            format!("{:<width$}", cell.text, width = widths[column])
        };
        out.push_str(&match cell.colour {
            Some(colour) => paint(&text, colour, colorize),
            None => text,
        });
        if !last {
            out.push_str("  ");
        }
    }
    out.push_str(suffix);
    out.push('\n');
    out
}

/// Machine-readable output: the query, the published documents (`null` when
/// none), and every section with each hit's node, slot facts, raw pin and
/// pin state, so a script can tell "unknown identity" from "unused".
fn render_json(reference: &ItemRef, report: &SearchReport) -> String {
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
    let doc = serde_json::json!({
        "query": { "name": reference.name, "tag": reference.tag },
        "contract": report.contract.as_ref().map(published_json),
        "pairing": report.pairing.as_ref().map(published_json),
        "implementers": implementers,
        "consumers": consumers,
        "participants": participants,
        "observers": observers,
    });
    format!("{doc}\n")
}

fn published_json(doc: &PublishedDoc) -> serde_json::Value {
    serde_json::json!({
        "repo_id": doc.repo_id,
        "repo_label": doc.repo_label,
        "path": doc.path,
        "sha256": doc.sha256.as_str(),
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
    use super::*;
    use config::node::Cardinality;
    use core_node::{Consumer, Implementer, Observer, Participant};
    use core_node_api::encoding::RepoSourceKind;
    use daemon_config::repository::ManifestFingerprint;

    const HUB: &str = "https://github.com/Peppy-bot/nodes-hub.git (ref: main)";
    const CONTRACTS: &str = "https://github.com/Peppy-bot/contracts-hub.git (ref: main)";

    fn reference() -> ItemRef {
        ItemRef::parse("rgb_camera:v1", "search").expect("a valid reference")
    }

    fn sha(fill: char) -> String {
        fill.to_string().repeat(64)
    }

    fn node(name: &str, path: &str) -> IndexedNode {
        IndexedNode {
            node_name: name.to_owned(),
            node_tag: "v1".to_owned(),
            repo_id: 1000,
            repo_label: HUB.to_owned(),
            source_type: RepoSourceKind::Git,
            path: path.to_owned(),
            shadowed_by: None,
        }
    }

    fn contract() -> PublishedDoc {
        PublishedDoc {
            repo_id: 1002,
            repo_label: CONTRACTS.to_owned(),
            path: "cameras/rgb_camera.json5".to_owned(),
            sha256: ManifestFingerprint::parse(&sha('a')).unwrap(),
        }
    }

    fn empty() -> SearchReport {
        SearchReport {
            contract: None,
            pairing: None,
            implementers: Vec::new(),
            consumers: Vec::new(),
            participants: Vec::new(),
            observers: Vec::new(),
            excluded_hint: String::new(),
        }
    }

    fn full() -> SearchReport {
        SearchReport {
            contract: Some(contract()),
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
            ..empty()
        }
    }

    /// Every section, with the published document first, rows aligned
    /// within a repository, the slot facts spelled as the manifest spells
    /// them, and the shadowing note where a lower-id repository wins.
    #[test]
    fn human_output_lists_every_section_with_aligned_columns() {
        let text = render_human(&reference(), &full(), false);

        let expected = [
            "rgb_camera:v1",
            "  contract published by https://github.com/Peppy-bot/contracts-hub.git (ref: main) at cameras/rgb_camera.json5 (sha256 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)",
            "",
            "Implemented by 2 indexed nodes",
            "  https://github.com/Peppy-bot/nodes-hub.git (ref: main):",
            "    NODE              TAG  SLOT    PIN                     PATH",
            "    uvc_camera_linux  v1   camera  pin aaaaaaaa (current)  uvc_camera/linux/peppy.json5",
            "    realsense_d4xx    v1   camera  unpinned                realsense_d4xx/peppy.json5",
            "",
            "Consumed by 1 indexed node",
            "  https://github.com/Peppy-bot/nodes-hub.git (ref: main):",
            "    NODE              TAG  SLOT                  PIN       PATH",
            "    episode_recorder  v1   camera (one_or_more)  unpinned  example_robot/episode_recorder/peppy.json5",
            "",
            "Pairing roles played by 1 indexed node",
            "  https://github.com/Peppy-bot/nodes-hub.git (ref: main):",
            "    NODE    TAG  ROLE    SLOT               PIN       PATH",
            "    viewer  v1   viewer  camera (optional)  unpinned  viewer/peppy.json5",
            "",
            "Observed by 1 indexed node",
            "  https://github.com/Peppy-bot/nodes-hub.git (ref: main):",
            "    NODE       TAG  ROLE    SLOT                 PIN                                              PATH",
            "    dashboard  v1   camera  watch (zero_or_one)  pin cccccccc (cached copy in /home/user/mirror)  dashboard/peppy.json5  (shadowed by /home/user/workspace)",
            "",
        ]
        .join("\n");
        assert_eq!(text, expected);
    }

    /// The two pin states a sync refuses are red, and only those: the
    /// column is padded on the plain text so colour never skews it.
    #[test]
    fn human_output_paints_the_pins_a_sync_would_refuse() {
        let report = SearchReport {
            contract: Some(contract()),
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
            ..empty()
        };

        let plain = render_human(&reference(), &report, false);
        assert!(
            plain.contains("    stale  v1   camera  pin bbbbbbbb (not in cache)"),
            "{plain}"
        );
        assert!(
            plain.contains(
                "    typo   v1   camera  unusable pin (fingerprint is not 64 hexadecimal characters: abc)  typo/peppy.json5"
            ),
            "{plain}"
        );
        assert!(!plain.contains('\x1b'), "no colour without a terminal");

        let coloured = render_human(&reference(), &report, true);
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
            "only the pin column is painted: {coloured}"
        );
    }

    /// An identity nobody publishes or uses: the header says nothing
    /// publishes it, the body says nothing uses it, and the excluded
    /// repositories are named as a possible reason.
    #[test]
    fn human_output_says_when_nothing_is_published_or_used() {
        let report = SearchReport {
            excluded_hint: ". 1 excluded repository (/tmp/private) is not indexed at all and may have provided it".to_owned(),
            ..empty()
        };

        let text = render_human(&reference(), &report, false);

        assert_eq!(
            text,
            "rgb_camera:v1\n\
             \x20 no configured repository publishes a contract or pairing named `rgb_camera:v1`; \
             check the name, or register its repository (`peppy repo add`) and run `peppy repo refresh`\n\
             \n\
             No indexed node implements, consumes, participates in, or observes `rgb_camera:v1`. \
             1 excluded repository (/tmp/private) is not indexed at all and may have provided it\n"
        );
    }

    /// Both namespaces are reported when the identity is published as a
    /// contract and as a pairing.
    #[test]
    fn human_output_names_both_published_documents() {
        let pairing = PublishedDoc {
            repo_id: 1002,
            repo_label: CONTRACTS.to_owned(),
            path: "pairings/rgb_camera.json5".to_owned(),
            sha256: ManifestFingerprint::parse(&sha('d')).unwrap(),
        };
        let report = SearchReport {
            contract: Some(contract()),
            pairing: Some(pairing),
            ..empty()
        };

        let text = render_human(&reference(), &report, false);

        assert!(
            text.contains("  contract published by https://github.com/Peppy-bot/contracts-hub.git (ref: main) at cameras/rgb_camera.json5 (sha256 aaaa"),
            "{text}"
        );
        assert!(
            text.contains("  pairing published by https://github.com/Peppy-bot/contracts-hub.git (ref: main) at pairings/rgb_camera.json5 (sha256 dddd"),
            "{text}"
        );
        assert!(!text.contains("no configured repository"), "{text}");
    }

    /// The JSON carries every section with the node, the slot facts, the
    /// raw pin and its state, and `null` for a document nobody publishes.
    #[test]
    fn json_output_carries_every_section() {
        let text = render_json(&reference(), &full());

        assert!(text.ends_with('\n'));
        let doc: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(
            doc["query"],
            serde_json::json!({ "name": "rgb_camera", "tag": "v1" })
        );
        assert_eq!(doc["contract"]["repo_id"], 1002);
        assert_eq!(doc["contract"]["sha256"], sha('a'));
        assert!(doc["pairing"].is_null());

        let implementers = doc["implementers"].as_array().expect("array");
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

        assert_eq!(doc["consumers"][0]["cardinality"], "one_or_more");
        assert_eq!(doc["participants"][0]["role"], "viewer");
        assert_eq!(doc["participants"][0]["optional"], true);

        let observer = &doc["observers"][0];
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
