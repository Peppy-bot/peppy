use super::common::{ARM_LINK_PAIRING, publish_repo_index, setup};
use config::consts::NODE_CONFIG_FILE;
use daemon_config::consts::PeppyDirs;
use peppy::commands::Command;
use peppy::commands::repo::{RepoCommand, RepoCommands, search_rendered};
use std::path::{Path, PathBuf};

const RGB_CAMERA_CONTRACT: &str = r#"{
  peppy_schema: "contract/v1",
  manifest: { name: "rgb_camera", tag: "v1" },
  interfaces: {}
}"#;

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

/// One fs repository publishing the `rgb_camera:v1` contract, the
/// `arm_link:v1` pairing, and a node for each way of using them, refreshed
/// into the emulated daemon's caches. Returns the Peppy home, the
/// repository root (canonical, as the caches record it) and the contract's
/// fingerprint.
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

/// A refreshed repository answers a contract search from the caches: the
/// published document, the implementer with its pin checked, and the
/// consumer with its cardinality, each at the manifest path refresh
/// recorded.
#[test]
fn repo_search_reports_a_contracts_implementers_and_consumers() {
    let (_rt, serve, ctx, _work_dir) = setup();
    let (peppy_dirs, repo_dir, contract_sha) = seeded(&serve, &ctx);

    let text = search_rendered(&peppy_dirs, "rgb_camera:v1", false).expect("searches");

    let label = repo_dir.to_string_lossy();
    assert!(
        text.starts_with(&format!(
            "rgb_camera:v1\n  contract published by {label} at {} (sha256 {contract_sha})\n",
            repo_dir.join("rgb_camera.json5").display()
        )),
        "{text}"
    );
    assert!(text.contains("\nImplemented by 1 indexed node\n"), "{text}");
    assert!(
        text.contains(&format!(
            "  {label}:\n    uvc_camera  v1  slot camera  pin {} (current)  {}\n",
            &contract_sha[..8],
            repo_dir.join("uvc_camera").join(NODE_CONFIG_FILE).display()
        )),
        "{text}"
    );
    assert!(text.contains("\nConsumed by 1 indexed node\n"), "{text}");
    assert!(
        text.contains(&format!(
            "    recorder  v1  slot frames (zero_or_one)  unpinned  {}\n",
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

/// A pairing search reports the roles played and observed, with the role
/// and slot facts the manifests declare.
#[test]
fn repo_search_reports_a_pairings_participants_and_observers() {
    let (_rt, serve, ctx, _work_dir) = setup();
    let (peppy_dirs, repo_dir, _) = seeded(&serve, &ctx);

    let text = search_rendered(&peppy_dirs, "arm_link:v1", false).expect("searches");

    assert!(
        text.contains(&format!(
            "  pairing published by {} at {} (sha256 {})\n",
            repo_dir.display(),
            repo_dir.join("arm_link.json5").display(),
            config::fingerprint::fingerprint_for_bytes(ARM_LINK_PAIRING.as_bytes())
        )),
        "{text}"
    );
    assert!(!text.contains("contract published"), "{text}");
    assert!(
        text.contains("\nPairing roles played by 1 indexed node\n"),
        "{text}"
    );
    assert!(
        text.contains("    follower_arm  v1  role arm  slot link (optional)  unpinned  "),
        "{text}"
    );
    assert!(text.contains("\nObserved by 1 indexed node\n"), "{text}");
    assert!(
        text.contains("    arm_logger  v1  role controller  slot watch (one)  unpinned  "),
        "{text}"
    );
    assert!(!text.contains("Implemented by"), "{text}");
    assert!(!text.contains("Consumed by"), "{text}");
}

/// The JSON form of the same search parses and carries each section.
#[test]
fn repo_search_json_carries_the_report() {
    let (_rt, serve, ctx, _work_dir) = setup();
    let (peppy_dirs, _repo_dir, contract_sha) = seeded(&serve, &ctx);

    let text = search_rendered(&peppy_dirs, "rgb_camera:v1", true).expect("searches");
    let doc: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");

    assert_eq!(doc["query"]["name"], "rgb_camera");
    assert_eq!(doc["contract"]["sha256"], contract_sha);
    assert_eq!(doc["contract"]["repo_id"], 1);
    assert!(doc["pairing"].is_null());
    assert_eq!(doc["implementers"][0]["node"]["node_name"], "uvc_camera");
    assert_eq!(doc["implementers"][0]["node"]["source_type"], "fs");
    assert_eq!(doc["implementers"][0]["sha256"], contract_sha);
    assert_eq!(doc["implementers"][0]["pin"]["status"], "current");
    assert_eq!(doc["consumers"][0]["node"]["node_name"], "recorder");
    assert_eq!(doc["consumers"][0]["cardinality"], "zero_or_one");
    assert_eq!(doc["participants"], serde_json::json!([]));
    assert_eq!(doc["observers"], serde_json::json!([]));

    let text = search_rendered(&peppy_dirs, "arm_link:v1", true).expect("searches");
    let doc: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    assert!(doc["contract"].is_null());
    assert_eq!(doc["pairing"]["repo_id"], 1);
    assert_eq!(doc["participants"][0]["role"], "arm");
    assert_eq!(doc["participants"][0]["optional"], true);
    assert_eq!(doc["observers"][0]["role"], "controller");
    assert_eq!(doc["observers"][0]["cardinality"], "one");
}

/// A reference without a tag is refused by name, before any cache is read.
#[test]
fn repo_search_refuses_a_reference_without_a_tag() {
    let (_rt, serve, _ctx, _work_dir) = setup();
    let peppy_dirs = PeppyDirs::new(serve.temp_dir());

    let err = search_rendered(&peppy_dirs, "rgb_camera", false)
        .expect_err("a bare name is not a reference")
        .to_string();

    assert!(
        err.contains("search reference `rgb_camera` must be written as `<name>:<tag>`"),
        "{err}"
    );
}

/// An identity nobody publishes or uses is an answer, not an error.
#[test]
fn repo_search_says_when_nobody_publishes_or_uses_an_identity() {
    let (_rt, serve, ctx, _work_dir) = setup();
    let (peppy_dirs, _repo_dir, _) = seeded(&serve, &ctx);

    let text = search_rendered(&peppy_dirs, "nobody:v1", false).expect("an empty answer");

    assert_eq!(
        text,
        "nobody:v1\n\
         \x20 no configured repository publishes a contract or pairing named `nobody:v1`; \
         check the name, or register its repository (`peppy repo add`) and run `peppy repo refresh`\n\
         \n\
         No indexed node implements, consumes, participates in, or observes `nobody:v1`\n"
    );
}
