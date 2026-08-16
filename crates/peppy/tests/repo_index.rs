use daemon_config::consts::REPOSITORY_INDEX_FILE;
use peppy::commands::repo::repo_index;
use std::path::Path;
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

    repo_index(Some(repo.clone()), false).expect("indexing should succeed");

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

    repo_index(Some(repo.clone()), false).expect("indexing should succeed");
    repo_index(Some(repo), true).expect("the index it just wrote should check out");
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
    repo_index(Some(unconstrained.clone()), false).expect("indexing should succeed");
    let err = repo_index(Some(unconstrained), true)
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
    repo_index(Some(constrained.clone()), false).expect("indexing should succeed");
    repo_index(Some(constrained), true)
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
    repo_index(Some(repo.clone()), false).expect("indexing should succeed");

    let err = repo_index(Some(repo), true).expect_err("a dead option must fail the check");
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

    repo_index(Some(repo.clone()), false).expect("first run");
    let first = std::fs::read_to_string(repo.join(REPOSITORY_INDEX_FILE)).unwrap();
    repo_index(Some(repo.clone()), false).expect("second run");
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
    repo_index(Some(repo.clone()), false).expect("indexing should succeed");
    write_contract(&repo.join("robot/wrist.json5"), "wrist_link", "v1");

    let err = repo_index(Some(repo), true).expect_err("an unlisted item must fail the check");

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

    let err = repo_index(Some(repo), true).expect_err("a missing index must fail the check");
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
        repo_index(Some(repo.clone()), false).expect_err("a contested identity must be refused");

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

    let err = repo_index(Some(file), false).expect_err("a file is not a repository");
    assert!(err.to_string().contains("not a directory"), "{err}");
}
