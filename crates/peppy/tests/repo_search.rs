use super::common::{ARM_LINK_PAIRING, publish_repo_index, setup};
use config::consts::NODE_CONFIG_FILE;
use daemon_config::consts::PeppyDirs;
use peppy::commands::Command;
use peppy::commands::repo::{RepoCommand, RepoCommands, search_rendered, show_rendered};
use std::path::{Path, PathBuf};

const RGB_CAMERA_CONTRACT: &str = r#"{
  peppy_schema: "contract/v1",
  manifest: { name: "rgb_camera", tag: "v1" },
  interfaces: {}
}"#;

/// A launcher deploying the fixture's `plain` node, so the repository
/// publishes an item of the one untagged kind.
const DEMO_LAUNCHER: &str = r#"{
  peppy_schema: "launcher/v1",
  deployments: [
    { source: { name: "plain:v1" }, instances: [{ instance_id: "plain_instance" }] }
  ]
}"#;

/// An MCP exposure over the fixture's contract, pinned to `contract_sha`:
/// the fifth indexed kind.
fn camera_surface_exposure(contract_sha: &str) -> String {
    format!(
        r#"{{
  peppy_schema: "mcp_exposure/v1",
  manifest: {{ name: "camera_surface", tag: "v1" }},
  server: {{ title: "Camera surface" }},
  targets: {{
    front_camera: {{
      contract: {{ name: "rgb_camera", tag: "v1", sha256: "{contract_sha}" }},
      services: [
        {{
          member: "video_stream_info",
          tool: "front_camera.info",
          description: "Report the camera's stream parameters.",
          operation: "read_only",
          deadline_ms: 2000,
        }},
      ],
    }},
  }},
}}"#
    )
}

/// A node manifest with `manifest_extra` spliced after the identity and
/// `interfaces` as given. Coverage of the links it declares is a sync-time
/// rule, so a manifest with empty `interfaces` publishes fine; an observer
/// slot is the one link the parser holds to consuming at least one topic.
fn write_node(dir: &Path, name: &str, manifest_extra: &str, interfaces: &str) {
    std::fs::create_dir_all(dir).expect("create node dir");
    std::fs::write(
        dir.join(NODE_CONFIG_FILE),
        format!(
            r#"{{
  peppy_schema: "node/v1",
  manifest: {{ name: "{name}", tag: "v1"{manifest_extra} }},
  interfaces: {interfaces},
  execution: {{ language: "rust", build_cmd: ["true"], run_cmd: ["true"] }},
}}"#
        ),
    )
    .expect("write peppy.json5");
}

/// One fs repository publishing every indexed kind: the `rgb_camera:v1`
/// contract, the `arm_link:v1` pairing, the `demo` launcher, the
/// `camera_surface:v1` MCP exposure, and a node for each way of using the
/// contract and pairing, refreshed into the emulated daemon's caches.
/// Returns the Peppy home, the repository root (canonical, as the caches
/// record it) and the contract's fingerprint.
fn seeded(
    serve: &peppy::test_support::ServeCommandEmulation,
    ctx: &std::sync::Arc<peppy::context::AppContext>,
) -> (PeppyDirs, PathBuf, String) {
    let peppy_dirs = PeppyDirs::new(serve.temp_dir());
    let repo_dir = serve.temp_dir().join("hub");
    std::fs::create_dir_all(&repo_dir).expect("create repo dir");
    let repo_dir = std::fs::canonicalize(&repo_dir).expect("canonical repo dir");

    std::fs::write(repo_dir.join("rgb_camera.json5"), RGB_CAMERA_CONTRACT).expect("contract");
    std::fs::write(repo_dir.join("arm_link.json5"), ARM_LINK_PAIRING).expect("pairing");
    let contract_sha = config::fingerprint::fingerprint_for_bytes(RGB_CAMERA_CONTRACT.as_bytes());
    std::fs::write(repo_dir.join("demo.json5"), DEMO_LAUNCHER).expect("launcher");
    std::fs::write(
        repo_dir.join("camera_surface.json5"),
        camera_surface_exposure(&contract_sha),
    )
    .expect("exposure");

    write_node(
        &repo_dir.join("uvc_camera"),
        "uvc_camera",
        &format!(
            r#", implements: [{{ name: "rgb_camera", tag: "v1", link_id: "camera", sha256: "{contract_sha}" }}]"#
        ),
        "{}",
    );
    write_node(
        &repo_dir.join("recorder"),
        "recorder",
        r#", depends_on: { contracts: [{ name: "rgb_camera", tag: "v1", link_id: "frames", cardinality: "zero_or_one" }] }"#,
        "{}",
    );
    write_node(
        &repo_dir.join("follower_arm"),
        "follower_arm",
        r#", depends_on: { pairings: [{ name: "arm_link", tag: "v1", role: "arm", link_id: "link", optional: true }] }"#,
        "{}",
    );
    write_node(
        &repo_dir.join("arm_logger"),
        "arm_logger",
        r#", depends_on: { pairing_observers: [{ name: "arm_link", tag: "v1", role: "controller", link_id: "watch" }] }"#,
        r#"{ topics: { consumes: [{ link_id: "watch", name: "joint_commands" }] } }"#,
    );
    write_node(&repo_dir.join("plain"), "plain", "", "{}");
    publish_repo_index(&repo_dir);

    let conf_dir = peppy_dirs.conf_dir();
    std::fs::create_dir_all(&conf_dir).expect("create conf dir");
    std::fs::write(
        conf_dir.join("repositories.json5"),
        serde_json::to_string(&serde_json::json!([
            { "id": 1, "type": "fs", "path": repo_dir.to_string_lossy() }
        ]))
        .expect("serialize repos"),
    )
    .expect("write repos file");

    RepoCommand {
        command: RepoCommands::Refresh,
    }
    .execute(ctx)
    .expect("repo refresh should succeed");

    (peppy_dirs, repo_dir, contract_sha)
}

/// A refreshed repository answers a contract show from the caches: the
/// published document, the implementer with its pin checked, and the
/// consumer with its cardinality, each at the manifest path refresh
/// recorded.
#[test]
fn repo_show_reports_a_contracts_implementers_and_consumers() {
    let (_rt, serve, ctx, _work_dir) = setup();
    let (peppy_dirs, repo_dir, contract_sha) = seeded(&serve, &ctx);

    let text = show_rendered(&peppy_dirs, "rgb_camera:v1", false, None).expect("shows");

    let label = repo_dir.to_string_lossy();
    assert!(
        text.starts_with(&format!(
            "rgb_camera:v1\n  contract rgb_camera:v1 published by {label} at {} (sha256 {contract_sha})\n",
            repo_dir.join("rgb_camera.json5").display()
        )),
        "{text}"
    );
    assert!(text.contains("\nImplemented by 1 indexed node\n"), "{text}");
    assert!(text.contains(&format!("  {label}:\n    ┌─")), "{text}");
    assert!(
        text.contains(&format!(
            "    │ uvc_camera │ v1  │ camera │ pin {} (current) │ {} │\n",
            &contract_sha[..8],
            repo_dir.join("uvc_camera").join(NODE_CONFIG_FILE).display()
        )),
        "{text}"
    );
    assert!(text.contains("\nConsumed by 1 indexed node\n"), "{text}");
    assert!(
        text.contains(&format!(
            "    │ recorder │ v1  │ frames (zero_or_one) │ unpinned │ {} │\n",
            repo_dir.join("recorder").join(NODE_CONFIG_FILE).display()
        )),
        "{text}"
    );
    assert!(!text.contains("Pairing roles played by"), "{text}");
    assert!(!text.contains("Observed by"), "{text}");
    assert!(
        !text.contains("plain"),
        "an unlinked node is not a hit: {text}"
    );
}

/// A pairing show reports the roles played and observed, with the role
/// and slot facts the manifests declare.
#[test]
fn repo_show_reports_a_pairings_participants_and_observers() {
    let (_rt, serve, ctx, _work_dir) = setup();
    let (peppy_dirs, repo_dir, _) = seeded(&serve, &ctx);

    let text = show_rendered(&peppy_dirs, "arm_link:v1", false, None).expect("shows");

    assert!(
        text.contains(&format!(
            "  pairing arm_link:v1 published by {} at {} (sha256 {})\n",
            repo_dir.display(),
            repo_dir.join("arm_link.json5").display(),
            config::fingerprint::fingerprint_for_bytes(ARM_LINK_PAIRING.as_bytes())
        )),
        "{text}"
    );
    assert!(!text.contains("contract arm_link"), "{text}");
    assert!(
        text.contains("\nPairing roles played by 1 indexed node\n"),
        "{text}"
    );
    assert!(
        text.contains("    │ follower_arm │ v1  │ arm  │ link (optional) │ unpinned │ "),
        "{text}"
    );
    assert!(text.contains("\nObserved by 1 indexed node\n"), "{text}");
    assert!(
        text.contains("    │ arm_logger │ v1  │ controller │ watch (one) │ unpinned │ "),
        "{text}"
    );
    assert!(!text.contains("Implemented by"), "{text}");
    assert!(!text.contains("Consumed by"), "{text}");
}

/// A bare name is a pattern matching any tag: the contract's report comes
/// back without the query naming `v1`.
#[test]
fn repo_show_a_bare_name_matches_any_tag() {
    let (_rt, serve, ctx, _work_dir) = setup();
    let (peppy_dirs, repo_dir, contract_sha) = seeded(&serve, &ctx);

    let text = show_rendered(&peppy_dirs, "rgb_camera", false, None).expect("shows");

    assert!(
        text.starts_with(&format!(
            "rgb_camera\n  contract rgb_camera:v1 published by {} at {} (sha256 {contract_sha})\n",
            repo_dir.display(),
            repo_dir.join("rgb_camera.json5").display()
        )),
        "{text}"
    );
    assert!(text.contains("\nImplemented by 1 indexed node\n"), "{text}");
}

/// A partial name lists every matching identity across the kinds, with
/// where each document is stored.
#[test]
fn repo_search_lists_partial_matches() {
    let (_rt, serve, ctx, _work_dir) = setup();
    let (peppy_dirs, repo_dir, contract_sha) = seeded(&serve, &ctx);

    let text = search_rendered(&peppy_dirs, "camera", false, None).expect("searches");

    assert!(
        text.starts_with("camera\n\n3 items match `camera`\n"),
        "{text}"
    );
    let exposure_sha = config::fingerprint::fingerprint_for_bytes(
        camera_surface_exposure(&contract_sha).as_bytes(),
    );
    assert!(
        text.contains("│ mcp exposure │ camera_surface:v1 │"),
        "{text}"
    );
    assert!(
        text.contains(&repo_dir.join("camera_surface.json5").display().to_string()),
        "{text}"
    );
    assert!(
        text.contains(&format!("│ {} │", &exposure_sha[..8])),
        "{text}"
    );
    assert!(
        text.contains("│ contract     │ rgb_camera:v1     │"),
        "{text}"
    );
    assert!(
        text.contains("│ node         │ uvc_camera:v1     │"),
        "{text}"
    );
    assert!(
        !text.contains("Implemented by"),
        "several identities carry no usage report: {text}"
    );
}

/// `repo show` prints a report block for every matched identity instead
/// of the table, in match order.
#[test]
fn repo_show_reports_every_match() {
    let (_rt, serve, ctx, _work_dir) = setup();
    let (peppy_dirs, _repo_dir, _) = seeded(&serve, &ctx);

    let text = show_rendered(&peppy_dirs, "camera", false, None).expect("shows");

    assert!(!text.contains("items match"), "{text}");
    let exposure = text
        .find("  mcp exposure camera_surface:v1 published by")
        .expect(&text);
    let contract = text
        .find("  contract rgb_camera:v1 published by")
        .expect(&text);
    let node = text.find("  node uvc_camera:v1 published by").expect(&text);
    assert!(
        exposure < contract && contract < node,
        "blocks come in match order: {text}"
    );
    assert!(
        text.contains("\nImplemented by 1 indexed node\n"),
        "the contract's block carries its sections: {text}"
    );
}

/// A launcher is found by its file stem, the one untagged kind: the
/// search lists its one row in the singular, and its published line is
/// the show's whole answer.
#[test]
fn repo_show_details_a_launcher() {
    let (_rt, serve, ctx, _work_dir) = setup();
    let (peppy_dirs, repo_dir, _) = seeded(&serve, &ctx);

    let listed = search_rendered(&peppy_dirs, "demo", false, None).expect("searches");
    assert!(listed.contains("\n1 item matches `demo`\n"), "{listed}");
    assert!(listed.contains("│ launcher │ demo │"), "{listed}");

    let text = show_rendered(&peppy_dirs, "demo", false, None).expect("shows");

    assert_eq!(
        text,
        format!(
            "demo\n  launcher demo published by {} at {} (sha256 {})\n",
            repo_dir.display(),
            repo_dir.join("demo.json5").display(),
            config::fingerprint::fingerprint_for_bytes(DEMO_LAUNCHER.as_bytes())
        )
    );
}

/// A digest query keeps only the copies carrying those bytes: the
/// published fingerprint finds the document, an unpublished one finds
/// nothing, which a search answers and a show refuses.
#[test]
fn repo_show_finds_a_pinned_digest() {
    let (_rt, serve, ctx, _work_dir) = setup();
    let (peppy_dirs, repo_dir, contract_sha) = seeded(&serve, &ctx);

    let pinned = format!("rgb_camera:v1@{contract_sha}");
    let text = show_rendered(&peppy_dirs, &pinned, false, None).expect("shows");
    assert!(
        text.starts_with(&format!(
            "{pinned}\n  contract rgb_camera:v1 published by {} at {} (sha256 {contract_sha})\n",
            repo_dir.display(),
            repo_dir.join("rgb_camera.json5").display()
        )),
        "{text}"
    );
    assert!(text.contains("\nImplemented by 1 indexed node\n"), "{text}");

    let absent = "f".repeat(64);
    let text = search_rendered(&peppy_dirs, &format!("rgb_camera:v1@{absent}"), false, None)
        .expect("an empty answer");
    assert!(
        text.contains("nothing in any configured repository's cache matches"),
        "{text}"
    );

    let err = show_rendered(&peppy_dirs, &format!("rgb_camera:v1@{absent}"), false, None)
        .expect_err("a show has no report to print")
        .to_string();
    assert!(
        err.contains("nothing in any configured repository's cache matches"),
        "{err}"
    );
}

/// The JSON form of a show parses and carries the matches and the
/// report.
#[test]
fn repo_show_json_carries_the_report() {
    let (_rt, serve, ctx, _work_dir) = setup();
    let (peppy_dirs, _repo_dir, contract_sha) = seeded(&serve, &ctx);

    let text = show_rendered(&peppy_dirs, "rgb_camera:v1", true, None).expect("shows");
    let doc: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");

    assert_eq!(doc["query"]["raw"], "rgb_camera:v1");
    assert_eq!(doc["query"]["name"], "rgb_camera");
    assert_eq!(doc["query"]["tag"], "v1");
    assert!(doc["query"]["sha256"].is_null());
    let matches = doc["matches"].as_array().expect("array");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["kind"], "contract");
    assert_eq!(matches[0]["exact"], true);
    assert_eq!(matches[0]["published"]["sha256"], contract_sha);
    assert_eq!(matches[0]["published"]["repo_id"], 1);
    assert_eq!(doc["reports"].as_array().expect("array").len(), 1);
    let report = &doc["reports"][0];
    assert_eq!(report["name"], "rgb_camera");
    assert_eq!(report["implementers"][0]["node"]["node_name"], "uvc_camera");
    assert_eq!(report["implementers"][0]["node"]["source_type"], "fs");
    assert_eq!(report["implementers"][0]["sha256"], contract_sha);
    assert_eq!(report["implementers"][0]["pin"]["status"], "current");
    assert_eq!(report["consumers"][0]["node"]["node_name"], "recorder");
    assert_eq!(report["consumers"][0]["cardinality"], "zero_or_one");
    assert_eq!(report["participants"], serde_json::json!([]));
    assert_eq!(report["observers"], serde_json::json!([]));

    let text = show_rendered(&peppy_dirs, "arm_link:v1", true, None).expect("shows");
    let doc: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    assert_eq!(doc["matches"][0]["kind"], "pairing");
    let report = &doc["reports"][0];
    assert_eq!(report["participants"][0]["role"], "arm");
    assert_eq!(report["participants"][0]["optional"], true);
    assert_eq!(report["observers"][0]["role"], "controller");
    assert_eq!(report["observers"][0]["cardinality"], "one");
}

/// The JSON of a search lists each kind with no usage report, and the
/// launcher's missing tag is `null`.
#[test]
fn repo_search_json_lists_matches() {
    let (_rt, serve, ctx, _work_dir) = setup();
    let (peppy_dirs, _repo_dir, _) = seeded(&serve, &ctx);

    let text = search_rendered(&peppy_dirs, "camera|demo", true, None).expect("searches");
    let doc: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");

    assert_eq!(
        doc.as_object().expect("object").len(),
        3,
        "a search carries `query`, `matches`, and `excluded`, nothing more: {doc}"
    );
    let kinds: Vec<&str> = doc["matches"]
        .as_array()
        .expect("array")
        .iter()
        .map(|m| m["kind"].as_str().expect("a kind"))
        .collect();
    assert_eq!(
        kinds,
        vec!["launcher", "mcp_exposure", "contract", "node"],
        "`demo` is an outright name match, so the launcher ranks first"
    );
    let launcher = &doc["matches"][0];
    assert_eq!(launcher["name"], "demo");
    assert_eq!(launcher["exact"], true);
    assert!(launcher["tag"].is_null(), "{launcher}");
}

/// A pattern that is not a regular expression is refused by name, before
/// any cache is read.
#[test]
fn repo_search_rejects_an_invalid_regex() {
    let (_rt, serve, _ctx, _work_dir) = setup();
    let peppy_dirs = PeppyDirs::new(serve.temp_dir());

    let err = search_rendered(&peppy_dirs, "rgb_camera[", false, None)
        .expect_err("an unclosed class is not a pattern")
        .to_string();

    assert!(
        err.contains("search pattern `rgb_camera[` is not a valid regular expression"),
        "{err}"
    );
}

/// What follows `@` must be a sha256 fingerprint, refused by name before
/// any cache is read.
#[test]
fn repo_search_rejects_a_bad_digest() {
    let (_rt, serve, _ctx, _work_dir) = setup();
    let peppy_dirs = PeppyDirs::new(serve.temp_dir());

    let err = search_rendered(&peppy_dirs, "rgb_camera@xyz", false, None)
        .expect_err("`xyz` is not a fingerprint")
        .to_string();

    assert!(err.contains("search digest `xyz`"), "{err}");
    assert!(err.contains("64 hexadecimal"), "{err}");
}

/// A query nothing matches is an answer, not an error.
#[test]
fn repo_search_says_when_nothing_matches() {
    let (_rt, serve, ctx, _work_dir) = setup();
    let (peppy_dirs, _repo_dir, _) = seeded(&serve, &ctx);

    let text = search_rendered(&peppy_dirs, "nobody:v1", false, None).expect("an empty answer");

    assert_eq!(
        text,
        "nobody:v1\n\
         \x20 nothing in any configured repository's cache matches `nobody:v1`; \
         check the pattern, or register its repository (`peppy repo add`) and run `peppy repo refresh`\n"
    );
}

/// A narrow width limit caps every table at that many columns: the
/// outline never exceeds it, and the over-long absolute manifest paths
/// wrap onto continuation lines instead of breaking the box.
#[test]
fn repo_show_wraps_tables_to_a_narrow_width() {
    let (_rt, serve, ctx, _work_dir) = setup();
    let (peppy_dirs, _repo_dir, _) = seeded(&serve, &ctx);

    let wide = show_rendered(&peppy_dirs, "rgb_camera:v1", false, None).expect("shows");
    let narrow = show_rendered(&peppy_dirs, "rgb_camera:v1", false, Some(60)).expect("shows");

    for line in narrow.lines() {
        if ['│', '┐', '┤', '┘'].iter().any(|c| line.ends_with(*c)) {
            assert!(line.chars().count() <= 60, "{line}\n{narrow}");
        }
    }
    let table_lines = |text: &str| {
        text.lines()
            .filter(|line| line.starts_with("    │"))
            .count()
    };
    assert!(
        table_lines(&narrow) > table_lines(&wide),
        "wrapping adds continuation lines:\n{narrow}"
    );
}
