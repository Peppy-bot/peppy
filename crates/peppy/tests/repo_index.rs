use daemon_config::consts::REPOSITORY_INDEX_FILE;
use peppy::commands::repo::{CheckScope, repo_index};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Helper: write a minimal valid node manifest under `dir`.
fn write_node(dir: &Path, name: &str, tag: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("peppy.json5"),
        format!(
            r#"{{
  peppy_schema: "node/v1",
  manifest: {{ name: "{name}", tag: "{tag}" }},
  interfaces: {{}},
  execution: {{ language: "rust", build_cmd: ["true"], run_cmd: ["true"] }},
}}"#
        ),
    )
    .unwrap();
}

/// Helper: write a minimal valid contract manifest at `path`.
fn write_contract(path: &Path, name: &str, tag: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        format!(
            r#"{{
  peppy_schema: "contract/v1",
  manifest: {{ name: "{name}", tag: "{tag}" }},
  interfaces: {{}}
}}"#
        ),
    )
    .unwrap();
}

#[test]
fn repo_index_writes_the_file() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("hub");
    write_node(&repo.join("nodes/uvc_camera"), "uvc_camera", "v1");
    write_contract(&repo.join("interfaces/rgb.json5"), "rgb_camera", "v1");

    repo_index(Some(repo.clone()), None).expect("indexing should succeed");

    let content = std::fs::read_to_string(repo.join(REPOSITORY_INDEX_FILE)).unwrap();
    assert!(
        content.contains(r#"peppy_schema: "repository/v1""#),
        "{content}"
    );
    assert!(
        content.contains("nodes/uvc_camera/peppy.json5"),
        "{content}"
    );
    assert!(content.contains("interfaces/rgb.json5"), "{content}");
}

#[test]
fn repo_index_check_passes_on_a_generated_index() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("hub");
    write_node(&repo.join("nodes/a"), "a", "v1");

    repo_index(Some(repo.clone()), None).expect("indexing should succeed");
    repo_index(Some(repo), Some(CheckScope::Index))
        .expect("the index it just wrote should check out");
}

/// Helper: a composed family whose one structurally broken member is the
/// one the given `constraints` refuse. The mujoco arm binds `observed` to
/// its engine as a scalar, so the recorder's append onto that slot cannot
/// flatten: without constraints the check must fail on `mujoco` + recorder,
/// with them it must pass, because a refused selection is refused by
/// design rather than required to flatten.
fn write_constrained_family(repo: &Path, constraints: &str) {
    std::fs::create_dir_all(repo.join("launchers")).unwrap();
    std::fs::write(
        repo.join("launchers/family.json5"),
        format!(
            r#"{{
  peppy_schema: "launcher/v1",
  components: [
    {{ name: "robot", default: "real",
       options: {{
           real: {{ deployments: [
               {{ source: {{ name: "arm", tag: "v1" }},
                  instances: [{{ instance_id: "arm_inst" }}] }} ] }},
           mujoco: {{ deployments: [
               {{ source: {{ name: "engine", tag: "v1" }},
                  instances: [{{ instance_id: "sim_inst" }}] }},
               {{ source: {{ name: "arm", tag: "v1" }},
                  instances: [{{ instance_id: "arm_inst",
                                links: {{ observed: "sim_inst" }} }}] }} ] }},
       }} }},
    {{ name: "recorder", optional: true,
       options: {{ on: {{
           deployments: [
               {{ source: {{ name: "recorder", tag: "v1" }},
                  instances: [{{ instance_id: "recorder_inst" }}] }} ],
           adjustments: [
               {{ target: "arm_inst",
                  add_links: {{ observed: ["recorder_inst"] }} }} ],
       }} }} }},
  ],
  {constraints}
  deployments: [],
}}"#
        ),
    )
    .unwrap();
}

/// The end-to-end shape of the whole feature: a family with a member that
/// flattens into nonsense is refused by its `constraints`, and
/// `repo index --check` holds only the legal members to flattening. The
/// same family without the constraints fails the same check on the same
/// member.
#[test]
fn repo_index_check_skips_selections_the_constraints_refuse() {
    let tmp = TempDir::new().unwrap();

    let unconstrained = tmp.path().join("unconstrained");
    write_node(&unconstrained.join("nodes/a"), "a", "v1");
    write_constrained_family(&unconstrained, "");
    repo_index(Some(unconstrained.clone()), None).expect("indexing should succeed");
    let err = repo_index(Some(unconstrained), Some(CheckScope::Index))
        .expect_err("the broken member must fail the unconstrained check");
    let message = err.to_string();
    assert!(message.contains("family.json5"), "{message}");
    assert!(
        message.contains("robot=mujoco  recorder=on"),
        "the check names the selection that cannot flatten: {message}"
    );

    let constrained = tmp.path().join("constrained");
    write_node(&constrained.join("nodes/a"), "a", "v1");
    write_constrained_family(
        &constrained,
        r#"constraints: [
    { when: { recorder: "on" }, requires: [{ robot: "real" }],
      reason: "the recorder films the physical rig" } ],"#,
    );
    repo_index(Some(constrained.clone()), None).expect("indexing should succeed");
    repo_index(Some(constrained), Some(CheckScope::Index))
        .expect("the constrained family should check out: its refused member need not flatten");
}

/// The guardrail on the guardrail: constraints that strangle an option out
/// of the family entirely fail the check, naming the dead option, so a
/// typo'd rule cannot silently shrink what a launcher offers.
#[test]
fn repo_index_check_flags_an_option_the_constraints_strangle() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("hub");
    write_node(&repo.join("nodes/a"), "a", "v1");
    write_constrained_family(
        &repo,
        r#"constraints: [
    { when: { robot: "mujoco" }, requires: [{ recorder: "on" }],
      reason: "the sim exists to produce datasets" },
    { when: { recorder: "on" }, requires: [{ robot: "real" }],
      reason: "the recorder films the physical rig" } ],"#,
    );
    repo_index(Some(repo.clone()), None).expect("indexing should succeed");

    let err = repo_index(Some(repo), Some(CheckScope::Index))
        .expect_err("a dead option must fail the check");
    let message = err.to_string();
    assert!(
        message.contains("fill axis `robot` with `mujoco`"),
        "the check names the option nothing can select: {message}"
    );
}

/// Writing twice produces the same file, so re-running generation after an
/// unrelated change never shows up as a spurious diff in a pull request.
#[test]
fn repo_index_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("hub");
    write_node(&repo.join("nodes/a"), "a", "v1");
    write_node(&repo.join("nodes/b"), "b", "v1");

    repo_index(Some(repo.clone()), None).expect("first run");
    let first = std::fs::read_to_string(repo.join(REPOSITORY_INDEX_FILE)).unwrap();
    repo_index(Some(repo.clone()), None).expect("second run");
    let second = std::fs::read_to_string(repo.join(REPOSITORY_INDEX_FILE)).unwrap();

    assert_eq!(first, second);
}

/// The mistake people will actually make: add an item, forget the index.
/// The check names the file and the identity, and says how to fix it.
#[test]
fn repo_index_check_names_the_unlisted_item() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("hub");
    write_node(&repo.join("nodes/a"), "a", "v1");
    repo_index(Some(repo.clone()), None).expect("indexing should succeed");
    write_contract(&repo.join("robot/wrist.json5"), "wrist_link", "v1");

    let err = repo_index(Some(repo), Some(CheckScope::Index))
        .expect_err("an unlisted item must fail the check");

    let message = err.to_string();
    assert!(message.contains("robot/wrist.json5"), "{message}");
    assert!(message.contains("wrist_link:v1"), "{message}");
    assert!(message.contains("peppy repo index"), "{message}");
}

/// A repository with no index does not check out, and the message names
/// the file that was looked for.
#[test]
fn repo_index_check_fails_when_the_index_is_missing() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("hub");
    write_node(&repo.join("nodes/a"), "a", "v1");

    let err = repo_index(Some(repo), Some(CheckScope::Index))
        .expect_err("a missing index must fail the check");
    assert!(err.to_string().contains(REPOSITORY_INDEX_FILE), "{err}");
}

/// Generation refuses a repository that claims one identity twice, on the
/// branch of the person who claimed it, naming both files.
#[test]
fn repo_index_refuses_an_identity_claimed_twice() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("hub");
    write_node(&repo.join("uvc/rust"), "uvc_camera", "v1");
    write_node(&repo.join("uvc/python"), "uvc_camera", "v1");

    let err =
        repo_index(Some(repo.clone()), None).expect_err("a contested identity must be refused");

    let message = err.to_string();
    assert!(message.contains("uvc_camera:v1"), "{message}");
    assert!(message.contains("uvc/rust/peppy.json5"), "{message}");
    assert!(message.contains("uvc/python/peppy.json5"), "{message}");
    assert!(
        !repo.join(REPOSITORY_INDEX_FILE).exists(),
        "no index is written when the repository contradicts itself"
    );
}

#[test]
fn repo_index_rejects_a_path_that_is_not_a_directory() {
    let tmp = TempDir::new().unwrap();
    let file = tmp.path().join("not_a_dir");
    std::fs::write(&file, "").unwrap();

    let err = repo_index(Some(file), None).expect_err("a file is not a repository");
    assert!(err.to_string().contains("not a directory"), "{err}");
}

// --- Exposure validation through `--check --validate-mcp-exposures`: the
// repository suite of the built-in MCP server plan. The hubs below are
// temporary; the contract caches are seeded directly, the way a
// `peppy repo refresh` of a registered contract repository would fill them.

mod exposures {
    use super::*;
    use daemon_config::consts::PeppyDirs;
    use peppy::commands::mcp::mcp_catalog_rendered;
    use peppy::commands::repo::check_index;

    const CAMERA_CONTRACT: &str = r#"{
        peppy_schema: "contract/v1",
        manifest: { name: "rgb_camera", tag: "v1" },
        interfaces: {
            topics: [
                {
                    name: "video_stream",
                    message_format: {
                        frame: { $type: "array", $items: "u8" },
                        encoding: "string",
                        width: "u16",
                        height: "u16",
                    },
                },
            ],
            services: [
                {
                    name: "set_brightness",
                    request_message_format: { value: "i32", label: "string" },
                    response_message_format: { applied: "bool" },
                },
            ],
        },
    }"#;

    /// A contract whose message format uses a field name the transport
    /// reserves: it cannot be converted to a public schema.
    const UNCONVERTIBLE_CONTRACT: &str = r#"{
        peppy_schema: "contract/v1",
        manifest: { name: "tagged_readings", tag: "v1" },
        interfaces: {
            services: [
                {
                    name: "read",
                    response_message_format: { instance_id: "u8" },
                },
            ],
        },
    }"#;

    fn service_exposure(name: &str, body: &str) -> String {
        format!(
            r#"{{
            peppy_schema: "mcp_exposure/v1",
            manifest: {{ name: "{name}", tag: "v1" }},
            server: {{ title: "{name}" }},
            targets: {{ front_camera: {{ {body} }} }},
        }}"#
        )
    }

    fn camera_ref(sha256: Option<&str>) -> String {
        let pin = sha256
            .map(|sha| format!(", sha256: \"{sha}\""))
            .unwrap_or_default();
        format!(r#"contract: {{ name: "rgb_camera", tag: "v1"{pin} }}"#)
    }

    fn brightness_tool(extra: &str) -> String {
        format!(
            r#"services: [
                {{
                    member: "set_brightness",
                    tool: "front_camera.set_brightness",
                    description: "Set.",
                    operation: "mutating",
                    deadline_ms: 5000{extra}
                }},
            ]"#
        )
    }

    /// The hub under check: every violation class beside one valid
    /// exposure. Returns the repository root.
    fn write_hub(tmp: &Path) -> PathBuf {
        let repo = tmp.join("hub");
        std::fs::create_dir_all(repo.join("exposures")).unwrap();
        let write = |file: &str, content: String| {
            std::fs::write(repo.join("exposures").join(file), content).unwrap();
        };
        write(
            "good.json5",
            service_exposure(
                "good",
                &format!("{}, {}", camera_ref(None), brightness_tool("")),
            ),
        );
        write(
            "missing_member.json5",
            service_exposure(
                "missing_member",
                &format!(
                    r#"{}, services: [
                    {{ member: "no_such_service", tool: "front_camera.nothing", description: "N.",
                      operation: "read_only", deadline_ms: 1000 }},
                ]"#,
                    camera_ref(None)
                ),
            ),
        );
        write(
            "wrong_kind.json5",
            service_exposure(
                "wrong_kind",
                &format!(
                    r#"{}, services: [
                    {{ member: "video_stream", tool: "front_camera.stream", description: "S.",
                      operation: "read_only", deadline_ms: 1000 }},
                ]"#,
                    camera_ref(None)
                ),
            ),
        );
        write(
            "bad_restrict.json5",
            service_exposure(
                "bad_restrict",
                &format!(
                    "{}, {}",
                    camera_ref(None),
                    brightness_tool(r#", restrict: { label: { min: 0 } }"#)
                ),
            ),
        );
        write(
            "size_policy.json5",
            service_exposure(
                "size_policy",
                &format!(
                    r#"{}, topics: [
                    {{ member: "video_stream", resource: "front_camera.frame", description: "F.",
                      freshness: {{ max_age_ms: 1000 }}, update: {{ max_hz: 10 }} }},
                ]"#,
                    camera_ref(None)
                ),
            ),
        );
        write(
            "bad_representation.json5",
            service_exposure(
                "bad_representation",
                &format!(
                    r#"{}, topics: [
                    {{ member: "video_stream", resource: "front_camera.frame", description: "F.",
                      freshness: {{ max_age_ms: 1000 }}, update: {{ max_hz: 10 }},
                      representation: {{ image: "jpeg", fields: {{ data: "pixels", encoding: "encoding", width: "width", height: "height" }} }},
                      max_result_bytes: 65536, on_oversize: "downscale" }},
                ]"#,
                    camera_ref(None)
                ),
            ),
        );
        write(
            "sha_mismatch.json5",
            service_exposure(
                "sha_mismatch",
                &format!(
                    "{}, {}",
                    camera_ref(Some(&"b".repeat(64))),
                    brightness_tool("")
                ),
            ),
        );
        write(
            "unresolvable.json5",
            service_exposure(
                "unresolvable",
                r#"contract: { name: "ghost", tag: "v1" }, services: [
                    { member: "haunt", tool: "front_camera.haunt", description: "H.",
                      operation: "read_only", deadline_ms: 1000 },
                ]"#,
            ),
        );
        write(
            "unconvertible.json5",
            service_exposure(
                "unconvertible",
                r#"contract: { name: "tagged_readings", tag: "v1" }, services: [
                    { member: "read", tool: "front_camera.read", description: "R.",
                      operation: "read_only", deadline_ms: 1000 },
                ]"#,
            ),
        );
        write(
            "duplicate_names.json5",
            format!(
                r#"{{
                peppy_schema: "mcp_exposure/v1",
                manifest: {{ name: "duplicate_names", tag: "v1" }},
                server: {{ title: "Duplicate" }},
                targets: {{
                    front_camera: {{
                        {},
                        topics: [
                            {{ member: "video_stream", resource: "front_camera.same", description: "F.",
                              freshness: {{ max_age_ms: 1000 }}, update: {{ max_hz: 10 }},
                              max_result_bytes: 65536, on_oversize: "reject" }},
                        ],
                        services: [
                            {{ member: "set_brightness", tool: "front_camera.same", description: "S.",
                              operation: "mutating", deadline_ms: 1000 }},
                        ],
                    }},
                }},
            }}"#,
                camera_ref(None)
            ),
        );
        repo_index(Some(repo.clone()), None).expect("the hub indexes");
        repo
    }

    /// Seeds the contract cache of `dirs` from the given documents, written
    /// under the home, the way a refresh of a registered contract
    /// repository would.
    fn seed_contract_cache(dirs: &PeppyDirs, contracts: &[(&str, &str)]) {
        let docs = dirs.root().join("contracts");
        std::fs::create_dir_all(dirs.cache_dir()).unwrap();
        std::fs::create_dir_all(&docs).unwrap();
        let paths: Vec<(&str, PathBuf)> = contracts
            .iter()
            .map(|(name, content)| {
                let path = docs.join(format!("{name}.json5"));
                std::fs::write(&path, content).unwrap();
                (*name, path)
            })
            .collect();
        let documents: Vec<_> = paths
            .iter()
            .map(|(name, path)| (*name, "v1", path.as_path()))
            .collect();
        core_node::test_support::seed_contract_cache(dirs, &documents);
    }

    /// Seeds the exposure cache of `dirs` with the named exposures of `repo`.
    fn seed_exposure_cache(dirs: &PeppyDirs, repo: &Path, names: &[&str]) {
        let paths: Vec<(&str, PathBuf)> = names
            .iter()
            .map(|name| (*name, repo.join("exposures").join(format!("{name}.json5"))))
            .collect();
        let documents: Vec<_> = paths
            .iter()
            .map(|(name, path)| (*name, "v1", path.as_path()))
            .collect();
        core_node::test_support::seed_exposure_cache(dirs, &documents);
    }

    #[test]
    fn every_violation_class_is_reported_at_once_and_the_valid_exposure_passes() {
        let tmp = TempDir::new().unwrap();
        let repo = write_hub(tmp.path());
        let dirs = PeppyDirs::new(tmp.path().join("home"));
        seed_contract_cache(
            &dirs,
            &[
                ("rgb_camera", CAMERA_CONTRACT),
                ("tagged_readings", UNCONVERTIBLE_CONTRACT),
            ],
        );

        // The structural check alone still passes: the documents are all
        // well-formed and listed.
        check_index(&repo, CheckScope::Index, &dirs).expect("the index matches the tree");

        let report = check_index(&repo, CheckScope::IndexAndMcpExposures, &dirs)
            .expect_err("the invalid exposures fail the check")
            .to_string();
        assert!(!report.contains("`good:v1`"), "{report}");
        for (id, phrase) in [
            ("missing_member:v1", "declares no such service"),
            ("wrong_kind:v1", "declares no such service"),
            ("bad_restrict:v1", "must name a numeric member"),
            (
                "size_policy:v1",
                "declare `max_result_bytes` and `on_oversize`",
            ),
            ("bad_representation:v1", "has no root member `pixels`"),
            ("sha_mismatch:v1", "not in contract cache"),
            ("unresolvable:v1", "contract `ghost:v1`"),
            ("unconvertible:v1", "reserved"),
            // A document naming one public name twice does not parse, so it
            // is listed by no index and reported by path.
            ("exposures/duplicate_names.json5", "declared more than once"),
        ] {
            assert!(
                report.contains(&format!("`{id}`")),
                "{id} missing from:\n{report}"
            );
            assert!(
                report.contains(phrase),
                "{id}: `{phrase}` missing from:\n{report}"
            );
        }
        assert!(report.contains("9 exposures do not validate"), "{report}");
    }

    #[test]
    fn a_contract_that_is_not_cached_is_an_error_naming_it_not_a_pass() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("hub");
        std::fs::create_dir_all(repo.join("exposures")).unwrap();
        std::fs::write(
            repo.join("exposures/good.json5"),
            service_exposure(
                "good",
                &format!("{}, {}", camera_ref(None), brightness_tool("")),
            ),
        )
        .unwrap();
        repo_index(Some(repo.clone()), None).expect("the hub indexes");
        let dirs = PeppyDirs::new(tmp.path().join("home"));

        let report = check_index(&repo, CheckScope::IndexAndMcpExposures, &dirs)
            .expect_err("nothing is cached")
            .to_string();
        assert!(report.contains("`good:v1`"), "{report}");
        assert!(report.contains("contract `rgb_camera:v1`"), "{report}");
        assert!(report.contains("peppy repo refresh"), "{report}");

        seed_contract_cache(&dirs, &[("rgb_camera", CAMERA_CONTRACT)]);
        check_index(&repo, CheckScope::IndexAndMcpExposures, &dirs)
            .expect("the exposure validates once its contract is cached");
    }

    #[test]
    fn the_catalog_command_prints_the_derived_bundle() {
        let tmp = TempDir::new().unwrap();
        let repo = write_hub(tmp.path());
        let dirs = PeppyDirs::new(tmp.path().join("home"));
        seed_contract_cache(&dirs, &[("rgb_camera", CAMERA_CONTRACT)]);
        seed_exposure_cache(&dirs, &repo, &["good", "missing_member"]);

        let rendered = mcp_catalog_rendered(&dirs, "good:v1").expect("the catalog derives");
        let catalog: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(catalog["bundle_format"], 1);
        assert_eq!(catalog["exposure"]["name"], "good");
        assert_eq!(catalog["tools"][0]["name"], "front_camera.set_brightness");
        assert_eq!(
            catalog["contracts"][0]["sha256"],
            config::fingerprint::fingerprint_for_bytes(CAMERA_CONTRACT.as_bytes())
        );

        let error = mcp_catalog_rendered(&dirs, "missing_member:v1")
            .expect_err("an invalid exposure has no catalog")
            .to_string();
        assert!(error.contains("no_such_service"), "{error}");
        let error = mcp_catalog_rendered(&dirs, "absent:v1")
            .expect_err("an unknown exposure")
            .to_string();
        assert!(error.contains("absent:v1"), "{error}");
        let error = mcp_catalog_rendered(&dirs, "good")
            .expect_err("a reference needs a tag")
            .to_string();
        assert!(error.contains("<name>:<tag>"), "{error}");
    }
}
