use core_node::{MatchedItem, PublishedDoc, SearchOutcome, SearchQuery};
use core_node_api::encoding::RepoItemKind;
use daemon_config::consts::PeppyDirs;

use super::identity_label;
use crate::commands::colors::{COUNT_COLOR, NODE_COLOR, paint};
use crate::commands::table::render_table;
use crate::error::{Error, Result};

/// `peppy repo search <query>`: every indexed item the query matches,
/// listed with the summary of its description and where each document is
/// stored. With `text`, the query also matches the descriptions.
pub(super) fn repo_search(query: &str, text: bool, json: bool) -> Result<()> {
    print!(
        "{}",
        search_rendered(
            &PeppyDirs::default(),
            query,
            text,
            json,
            crate::terminal::stdout_width()
        )?
    );
    Ok(())
}

/// The search as the command prints it, against the Peppy home it names,
/// so a test reads the text instead of capturing stdout. `max_width` caps
/// the human table at that many columns (the command passes the
/// terminal's width); `None` keeps every row on one line, which is what
/// piped output gets.
pub fn search_rendered(
    peppy_dirs: &PeppyDirs,
    query: &str,
    text: bool,
    json: bool,
    max_width: Option<usize>,
) -> Result<String> {
    let query = SearchQuery::parse(query).map_err(Error::ExecutionFailed)?;
    let query = match text {
        true => query.with_descriptions(),
        false => query,
    };
    let outcome =
        core_node::search_repo_items(peppy_dirs, &query).map_err(Error::ExecutionFailed)?;
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

/// The outcome as a person reads it: one table row per match in the
/// service's rank order, or one line saying nothing matches. Tinted with
/// `stack list`'s palette: item identities cyan, counts green.
fn render_human(
    query: &SearchQuery,
    outcome: &SearchOutcome,
    colorize: bool,
    max_width: Option<usize>,
) -> String {
    let mut out = format!("{}\n", paint(colorize, NODE_COLOR, query.raw()));
    if outcome.matches.is_empty() {
        out.push_str(&format!(
            "  {}\n",
            no_match_phrase(query, &outcome.excluded_hint)
        ));
    } else {
        out.push_str(&match_table(query, &outcome.matches, colorize, max_width));
    }
    out
}

/// What both commands say about a query nothing matches: `repo search`
/// prints it as an answer, `repo show` refuses with it.
pub(super) fn no_match_phrase(query: &SearchQuery, excluded_hint: &str) -> String {
    format!(
        "nothing in any configured repository's cache matches `{}`; check the \
         pattern, or register its repository (`peppy repo add`) and run \
         `peppy repo refresh`{}",
        query.raw(),
        excluded_hint
    )
}

/// The listed view, one row per match in the service's rank order: the
/// kind, the identity, the summary of its description, and where the
/// document is stored, with the fingerprint shortened the way `repo show`'s
/// pin cells shorten it (the JSON carries it whole).
fn match_table(
    query: &SearchQuery,
    matches: &[MatchedItem],
    colorize: bool,
    max_width: Option<usize>,
) -> String {
    let phrase = match matches.len() {
        1 => "item matches",
        _ => "items match",
    };
    let mut out = format!(
        "\n{} {phrase} `{}`\n",
        paint(colorize, COUNT_COLOR, &matches.len().to_string()),
        query.raw()
    );
    let rows: Vec<Vec<String>> = matches
        .iter()
        .map(|item| {
            vec![
                kind_label(item.kind).to_owned(),
                paint(colorize, NODE_COLOR, &display_id(item)),
                summary(item).unwrap_or_default().to_owned(),
                item.published.repo_label.clone(),
                item.published.path.clone(),
                short_fingerprint(item.published.sha256.as_str()),
            ]
        })
        .collect();
    let mut table = String::new();
    render_table(
        &mut table,
        &["KIND", "ITEM", "SUMMARY", "REPOSITORY", "PATH", "SHA256"],
        &[rows],
        max_width,
    );
    out.push_str(table.trim_end_matches('\n'));
    out.push('\n');
    out
}

/// The first paragraph of the description of the match, or `None` for an
/// MCP exposure, the one kind with no description.
fn summary(item: &MatchedItem) -> Option<&str> {
    let descriptions = item.descriptions.as_ref()?;
    Some(descriptions.summary().as_str())
}

/// `name:tag`, or the bare name for untagged kinds (launchers).
pub(super) fn display_id(item: &MatchedItem) -> String {
    identity_label(&item.name, &item.tag)
}

/// The kind as prose. The JSON output uses [`RepoItemKind::as_str`]
/// instead, whose `mcp_exposure` stays a machine token.
pub(super) fn kind_label(kind: RepoItemKind) -> &'static str {
    match kind {
        RepoItemKind::McpExposure => "mcp exposure",
        other => other.as_str(),
    }
}

/// The first eight characters of a fingerprint, as every human cell shows
/// one; the JSON carries fingerprints whole.
pub(super) fn short_fingerprint(sha256: &str) -> String {
    sha256.chars().take(8).collect()
}

/// Machine-readable output: the parsed query, every match with its full
/// fingerprint, and the excluded-repositories hint.
fn render_json(query: &SearchQuery, outcome: &SearchOutcome) -> String {
    let doc = serde_json::json!({
        "query": query_json(query),
        "matches": matches_json(&outcome.matches),
        "excluded": outcome.excluded_hint,
    });
    format!("{doc}\n")
}

/// The query's parsed parts, as both commands' JSON carries them.
pub(super) fn query_json(query: &SearchQuery) -> serde_json::Value {
    serde_json::json!({
        "raw": query.raw(),
        "name": query.name_pattern(),
        "tag": query.tag_pattern(),
        "sha256": query.digest().map(|digest| digest.as_str()),
        "text": query.matches_descriptions(),
    })
}

/// Every match with its full fingerprint, as both commands' JSON lists
/// them.
pub(super) fn matches_json(matches: &[MatchedItem]) -> serde_json::Value {
    let matches: Vec<serde_json::Value> = matches
        .iter()
        .map(|item| {
            serde_json::json!({
                "kind": item.kind.as_str(),
                "name": item.name,
                "tag": match item.tag.is_empty() {
                    true => serde_json::Value::Null,
                    false => item.tag.clone().into(),
                },
                "by_name": item.by_name,
                "exact": item.exact,
                "summary": summary(item),
                "published": published_json(&item.published),
            })
        })
        .collect();
    matches.into()
}

fn published_json(doc: &PublishedDoc) -> serde_json::Value {
    serde_json::json!({
        "repo_id": doc.repo_id,
        "repo_label": doc.repo_label,
        "path": doc.path,
        "sha256": doc.sha256.as_str(),
    })
}

/// The service values the render tests of both commands feed in: one
/// repository label per hub, one node, one published contract, and any
/// other published document.
#[cfg(test)]
pub(super) mod fixtures {
    use core_node::{
        CachedDescriptions, IndexedNode, MatchedItem, PublishedDoc, SearchQuery, SearchReport,
    };
    use core_node_api::encoding::{RepoItemKind, RepoSourceKind};
    use daemon_config::repository::ManifestFingerprint;

    pub const HUB: &str = "https://github.com/Peppy-bot/nodes-hub.git (ref: main)";
    pub const CONTRACTS: &str = "https://github.com/Peppy-bot/contracts-hub.git (ref: main)";
    pub const LAUNCHERS: &str = "https://github.com/Peppy-bot/launchers-hub.git (ref: main)";

    pub fn query(raw: &str) -> SearchQuery {
        SearchQuery::parse(raw).expect("a valid query")
    }

    pub fn sha(fill: char) -> String {
        fill.to_string().repeat(64)
    }

    pub fn node(name: &str, path: &str) -> IndexedNode {
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

    /// A document stored at `path` in the repository `repo_id`, whose
    /// fingerprint repeats `fill`.
    pub fn published(repo_id: u32, repo_label: &str, path: &str, fill: char) -> PublishedDoc {
        PublishedDoc {
            repo_id,
            repo_label: repo_label.to_owned(),
            path: path.to_owned(),
            sha256: ManifestFingerprint::parse(&sha(fill)).unwrap(),
        }
    }

    pub fn contract() -> PublishedDoc {
        published(1002, CONTRACTS, "cameras/rgb_camera.json5", 'a')
    }

    /// The descriptions of a document whose paragraphs are `paragraphs`,
    /// with no member.
    pub fn described(paragraphs: &[&str]) -> CachedDescriptions {
        serde_json::from_value(serde_json::json!({ "description": paragraphs }))
            .expect("the paragraphs are not blank")
    }

    /// A match by name, whose description says what `name` is; an MCP
    /// exposure has none.
    pub fn matched(
        kind: RepoItemKind,
        name: &str,
        tag: &str,
        published: PublishedDoc,
    ) -> MatchedItem {
        let descriptions = match kind {
            RepoItemKind::McpExposure => None,
            _ => Some(described(&[&format!("What {name} is.")])),
        };
        MatchedItem {
            kind,
            name: name.to_owned(),
            tag: tag.to_owned(),
            by_name: true,
            exact: true,
            published,
            descriptions,
        }
    }

    pub fn empty_report() -> SearchReport {
        SearchReport {
            name: "rgb_camera".to_owned(),
            tag: "v1".to_owned(),
            implementers: Vec::new(),
            consumers: Vec::new(),
            participants: Vec::new(),
            observers: Vec::new(),
            pairing_slots: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn multi() -> SearchOutcome {
        SearchOutcome {
            matches: vec![
                MatchedItem {
                    exact: false,
                    ..matched(
                        RepoItemKind::Launcher,
                        "camera_boot",
                        "",
                        published(1001, LAUNCHERS, "camera_boot.json5", 'e'),
                    )
                },
                MatchedItem {
                    exact: false,
                    ..matched(RepoItemKind::Contract, "rgb_camera", "v1", contract())
                },
                MatchedItem {
                    exact: false,
                    ..matched(
                        RepoItemKind::Node,
                        "uvc_camera",
                        "v1",
                        published(1000, HUB, "uvc_camera/peppy.json5", 'b'),
                    )
                },
            ],
            excluded_hint: String::new(),
        }
    }

    /// A query nothing matches: one line says so, and the excluded
    /// repositories are named as a possible reason.
    #[test]
    fn human_output_says_when_nothing_matches() {
        let outcome = SearchOutcome {
            matches: Vec::new(),
            excluded_hint: ". 1 excluded repository (/tmp/private) is not indexed at all and may have provided it".to_owned(),
        };

        let text = render_human(&query("rgb_camera:v1"), &outcome, false, None);

        assert_eq!(
            text,
            "rgb_camera:v1\n\
             \x20 nothing in any configured repository's cache matches `rgb_camera:v1`; \
             check the pattern, or register its repository (`peppy repo add`) and run `peppy repo refresh`. \
             1 excluded repository (/tmp/private) is not indexed at all and may have provided it\n"
        );
    }

    /// Several matched identities are one flat table in the service's
    /// rank order, with the summary of each description and the
    /// fingerprints shortened.
    #[test]
    fn human_output_lists_multiple_matches_as_a_table() {
        let text = render_human(&query("camera"), &multi(), false, None);

        let expected = [
            "camera",
            "",
            "3 items match `camera`",
            "┌──────────┬───────────────┬──────────────────────┬────────────────────────────────────────────────────────────┬──────────────────────────┬──────────┐",
            "│ KIND     │ ITEM          │ SUMMARY              │ REPOSITORY                                                 │ PATH                     │ SHA256   │",
            "├──────────┼───────────────┼──────────────────────┼────────────────────────────────────────────────────────────┼──────────────────────────┼──────────┤",
            "│ launcher │ camera_boot   │ What camera_boot is. │ https://github.com/Peppy-bot/launchers-hub.git (ref: main) │ camera_boot.json5        │ eeeeeeee │",
            "│ contract │ rgb_camera:v1 │ What rgb_camera is.  │ https://github.com/Peppy-bot/contracts-hub.git (ref: main) │ cameras/rgb_camera.json5 │ aaaaaaaa │",
            "│ node     │ uvc_camera:v1 │ What uvc_camera is.  │ https://github.com/Peppy-bot/nodes-hub.git (ref: main)     │ uvc_camera/peppy.json5   │ bbbbbbbb │",
            "└──────────┴───────────────┴──────────────────────┴────────────────────────────────────────────────────────────┴──────────────────────────┴──────────┘",
            "",
        ]
        .join("\n");
        assert_eq!(text, expected, "\n{text}");
    }

    /// One match is the same listed view, phrased in the singular.
    #[test]
    fn human_output_phrases_a_single_match() {
        let outcome = SearchOutcome {
            matches: vec![matched(
                RepoItemKind::Contract,
                "rgb_camera",
                "v1",
                contract(),
            )],
            excluded_hint: String::new(),
        };

        let text = render_human(&query("rgb_camera:v1"), &outcome, false, None);

        assert!(
            text.contains("\n1 item matches `rgb_camera:v1`\n"),
            "{text}"
        );
        assert!(text.contains("│ contract │ rgb_camera:v1 │"), "{text}");
    }

    /// The match table tints identities cyan and the count green, and
    /// respects the width limit like every other table.
    #[test]
    fn human_output_tints_and_wraps_the_match_table() {
        use crate::commands::colors::RESET;

        let coloured = render_human(&query("camera"), &multi(), true, None);
        assert!(
            coloured.contains(&format!("{COUNT_COLOR}3{RESET} items match `camera`")),
            "{coloured}"
        );
        assert!(
            coloured.contains(&format!("│ {NODE_COLOR}rgb_camera:v1{RESET} ")),
            "{coloured}"
        );

        let narrow = render_human(&query("camera"), &multi(), false, Some(60));
        for line in narrow.lines() {
            if ['│', '┐', '┤', '┘'].iter().any(|c| line.ends_with(*c)) {
                assert!(line.chars().count() <= 60, "{line}\n{narrow}");
            }
        }
    }

    fn matches_of(doc: &serde_json::Value) -> &Vec<serde_json::Value> {
        doc["matches"].as_array().expect("array")
    }

    /// A match that a description search found by a description alone says
    /// so in the JSON, and an MCP exposure has no summary: its cell stays
    /// empty.
    #[test]
    fn a_match_by_description_and_an_exposure_with_no_summary() {
        let outcome = SearchOutcome {
            matches: vec![
                MatchedItem {
                    by_name: false,
                    exact: false,
                    descriptions: Some(described(&["A camera of the wrist.", "In meters."])),
                    ..matched(RepoItemKind::Contract, "rgb_camera", "v1", contract())
                },
                matched(
                    RepoItemKind::McpExposure,
                    "cameras",
                    "v1",
                    published(1003, HUB, "cameras.json5", 'c'),
                ),
            ],
            excluded_hint: String::new(),
        };
        let wrist = query("wrist").with_descriptions();
        let doc: serde_json::Value =
            serde_json::from_str(&render_json(&wrist, &outcome)).expect("valid JSON");
        assert_eq!(doc["query"]["text"], true);
        assert_eq!(matches_of(&doc)[0]["by_name"], false);
        assert_eq!(matches_of(&doc)[0]["summary"], "A camera of the wrist.");
        assert!(matches_of(&doc)[1]["summary"].is_null());

        let text = render_human(&wrist, &outcome, false, None);
        assert!(
            text.contains("│ contract     │ rgb_camera:v1 │ A camera of the wrist. │"),
            "{text}"
        );
        assert!(
            text.contains("│ mcp exposure │ cameras:v1    │                        │"),
            "{text}"
        );
    }

    /// The JSON carries the parsed query and every match with its full
    /// fingerprint and its summary; an untagged launcher's tag is `null`,
    /// and no usage report rides a search.
    #[test]
    fn json_output_lists_matches() {
        let text = render_json(&query("camera"), &multi());

        assert!(text.ends_with('\n'));
        let doc: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(
            doc["query"],
            serde_json::json!({
                "raw": "camera",
                "name": "camera",
                "tag": serde_json::Value::Null,
                "sha256": serde_json::Value::Null,
                "text": false,
            })
        );
        assert_eq!(matches_of(&doc)[1]["summary"], "What rgb_camera is.");
        assert_eq!(matches_of(&doc)[1]["by_name"], true);
        let matches = doc["matches"].as_array().expect("array");
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0]["kind"], "launcher");
        assert!(matches[0]["tag"].is_null());
        assert_eq!(matches[0]["exact"], false);
        assert_eq!(matches[1]["kind"], "contract");
        assert_eq!(matches[1]["published"]["repo_id"], 1002);
        assert_eq!(matches[1]["published"]["sha256"], sha('a'));
        assert_eq!(matches[2]["kind"], "node");
        assert_eq!(matches[2]["published"]["repo_label"], HUB);
        assert_eq!(doc["excluded"], "");
        assert_eq!(
            doc.as_object().expect("object").len(),
            3,
            "a search carries `query`, `matches`, and `excluded`, nothing more"
        );
    }
}
