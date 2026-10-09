//! `peppy stack resolve` holds a launcher to the launch checks of daemon
//! scopes with a cold nodes cache: the checks read the launcher and the
//! exposure documents alone, so a machine that holds no node, and no
//! contract, still refuses every scope a launch would refuse.

use daemon_config::consts::PeppyDirs;
use peppy::commands::stack::resolve_rendered;
use std::fs;
use std::path::{Path, PathBuf};

/// The design's `framework/framework_controls.json5`, kept beside the
/// registry of the daemon interfaces whose `stack_copies:v1` it names.
const FRAMEWORK_CONTROLS: &str = include_str!(
    "../../daemon-config-internal/src/daemon_interface/fixtures/framework_controls.json5"
);

/// The scope `simulation_mcp` gives its framework endpoint.
const SCOPE: &str = r#"{
    max_copies: 4,
    options: [
        { option: "openarm_sim", description: "A simulated OpenArm v2 standing on the floor" },
        { option: "so101_sim", description: "A simulated SO-101 clamped on the edge of a table" },
    ],
}"#;

/// A peppy home whose exposure cache holds `framework_controls:v1`, and
/// `unserved_controls:v1`, the same document naming a tag of `stack_copies`
/// this peppy does not serve. Its nodes and contracts caches are empty.
fn cold_home() -> (PeppyDirs, tempfile::TempDir) {
    let root = tempfile::tempdir().expect("temp peppy root");
    let docs = root.path().join("exposures");
    fs::create_dir_all(&docs).expect("exposure dir");
    let framework = docs.join("framework_controls.json5");
    fs::write(&framework, FRAMEWORK_CONTROLS).expect("exposure");
    let unserved = docs.join("unserved_controls.json5");
    fs::write(
        &unserved,
        FRAMEWORK_CONTROLS
            .replace(
                r#"name: "framework_controls""#,
                r#"name: "unserved_controls""#,
            )
            .replace(
                r#"daemon: { name: "stack_copies", tag: "v1" }"#,
                r#"daemon: { name: "stack_copies", tag: "v2" }"#,
            ),
    )
    .expect("exposure");
    let dirs = PeppyDirs::new(root.path());
    fs::create_dir_all(dirs.cache_dir()).expect("cache dir");
    core_node::test_support::seed_exposure_cache(
        &dirs,
        &[
            ("framework_controls", "v1", framework.as_path()),
            ("unserved_controls", "v1", unserved.as_path()),
        ],
    );
    (dirs, root)
}

/// The shape of `simulation_mcp`: the framework's endpoint under the
/// `robot_control` option, the robots on an axis that runs as copies, and
/// the top-level adjustments `adjustments`. `endpoint` is the instance
/// entry of the endpoint.
fn launcher(root: &Path, endpoint: &str, adjustments: &str) -> PathBuf {
    let path = root.join("simulation_mcp.json5");
    fs::write(
        &path,
        format!(
            r#"{{
            peppy_schema: "launcher/v1",
            core_nodes: ["robot_pc"],
            components: [
                {{ name: "robot_control", cardinality: "one", options: {{
                    robot_control: {{ deployments: [
                        {{ source: {{ exposures: ["framework_controls:v1"] }}, instances: [
                            {endpoint}
                        ] }}
                    ] }},
                    none: {{}},
                }} }},
                {{ name: "robot", cardinality: "zero_or_more", options: {{
                    openarm_sim: {{ deployments: [
                        {{ source: {{ name: "openarm", tag: "v1" }}, instances: [
                            {{ instance_id: "arm_inst" }}
                        ] }}
                    ] }},
                    so101_sim: {{ deployments: [
                        {{ source: {{ name: "so101", tag: "v1" }}, instances: [
                            {{ instance_id: "arm_inst" }}
                        ] }}
                    ] }},
                }} }},
            ],
            deployments: [{{ robot_control: "robot_control" }}],
            adjustments: [{adjustments}],
        }}"#
        ),
    )
    .expect("launcher");
    path
}

const ENDPOINT: &str = r#"{ instance_id: "framework_controls_inst", arguments: { port: 8903 } }"#;

fn scoped(scope: &str) -> String {
    format!(r#"{{ target: "framework_controls_inst", set_daemon_scopes: {{ stack: {scope} }} }}"#)
}

fn refusal(dirs: &PeppyDirs, launcher: PathBuf) -> String {
    resolve_rendered(dirs, launcher, &[], &[])
        .expect_err("the launcher is refused")
        .to_string()
}

#[test]
fn a_scoped_daemon_target_resolves_with_a_cold_nodes_cache() {
    let (dirs, root) = cold_home();
    let (document, report) = resolve_rendered(
        &dirs,
        launcher(root.path(), ENDPOINT, &scoped(SCOPE)),
        &[],
        &[],
    )
    .expect("the scope holds");
    let report = report.join("\n");
    assert!(
        report.contains(
            "daemon scopes hold: 1 daemon target(s) scoped and placed on the coordinator"
        ),
        "{report}"
    );
    assert!(
        report.contains("link rules not checked: the nodes cache is empty"),
        "{report}"
    );
    assert!(
        document.contains("daemon_scopes") && document.contains("so101_sim"),
        "the flat launcher carries the scope: {document}"
    );
}

#[test]
fn a_daemon_target_without_a_scope_is_refused() {
    let (dirs, root) = cold_home();
    let error = refusal(
        &dirs,
        launcher(
            root.path(),
            ENDPOINT,
            r#"{ target: "framework_controls_inst", set_arguments: { port: 8904 } }"#,
        ),
    );
    assert!(
        error.contains("the flat launcher breaks daemon scope rules a launch would reject")
            && error.contains(
                "daemon target `stack` (stack_copies:v1) of instance `framework_controls_inst` \
                 has no scope"
            ),
        "{error}"
    );
}

#[test]
fn a_scope_that_does_not_parse_or_names_no_copy_option_is_refused() {
    let (dirs, root) = cold_home();
    let error = refusal(
        &dirs,
        launcher(
            root.path(),
            ENDPOINT,
            &scoped(r#"{ max_copies: 4, options: [], model: "so101" }"#),
        ),
    );
    assert!(
        error.contains("does not parse") && error.contains("unknown field `model`"),
        "{error}"
    );

    let error = refusal(
        &dirs,
        launcher(
            root.path(),
            ENDPOINT,
            &scoped(r#"{ max_copies: 4, options: [{ option: "none", description: "Nothing" }] }"#),
        ),
    );
    assert!(
        error.contains(
            "`none` is not an option of an axis this launcher runs as copies; the copies it can \
             add:\n  - robot: `openarm_sim`, `so101_sim`"
        ),
        "{error}"
    );
}

#[test]
fn scopes_and_links_of_the_wrong_instance_or_target_are_refused() {
    let (dirs, root) = cold_home();
    let error = refusal(
        &dirs,
        launcher(
            root.path(),
            r#"{ instance_id: "framework_controls_inst", core_node: "robot_pc",
                 links: { stack: "framework_controls_inst" } }"#,
            &scoped(SCOPE),
        ),
    );
    for expected in [
        "instance `framework_controls_inst` serves daemon target `stack` and declares \
         `core_node: \"robot_pc\"`",
        "`links.stack` of instance `framework_controls_inst` names daemon target `stack`, which \
         takes no link",
    ] {
        assert!(
            error.contains(expected),
            "{expected}\nmissing from: {error}"
        );
    }

    // A scope key that names no daemon target of the instance; the one the
    // top-level adjustment replaces holds.
    let stray = launcher(
        root.path(),
        r#"{ instance_id: "framework_controls_inst", arguments: { port: 8903 },
             daemon_scopes: { stack: {}, recorder: {} } }"#,
        &scoped(SCOPE),
    );
    let error = refusal(&dirs, stray);
    assert!(
        error.contains(
            "`daemon_scopes.recorder` of instance `framework_controls_inst` names no daemon \
             target of its exposures; its daemon targets: `stack`"
        ) && !error.contains("does not parse"),
        "{error}"
    );
}

#[test]
fn a_scope_written_through_a_joined_copy_is_refused() {
    let (dirs, root) = cold_home();
    let path = launcher(
        root.path(),
        ENDPOINT,
        &format!(
            r#"{}, {{ target: "framework_controls_inst", when: {{ robot: "openarm_sim" }},
                   set_daemon_scopes: {{ stack: {SCOPE} }} }}"#,
            scoped(SCOPE)
        ),
    );
    let error = resolve_rendered(
        &dirs,
        path,
        &[],
        &[core_node_api::encoding::LaunchJoin {
            option: "openarm_sim".to_owned(),
            name: config::runtime::Name::new("alpha").expect("a name"),
        }],
    )
    .expect_err("the copy sets a scope")
    .to_string();
    assert!(
        error.contains(
            "copy `alpha` of option `openarm_sim` sets the scope of daemon target `stack` on \
             instance `framework_controls_inst`"
        ) && error.contains("a scope belongs to the stack")
            && !error.contains("would change"),
        "{error}"
    );
}

#[test]
fn a_daemon_interface_this_peppy_does_not_serve_is_refused_by_resolve() {
    let (dirs, root) = cold_home();
    let path = launcher(root.path(), ENDPOINT, &scoped(SCOPE));
    let text = fs::read_to_string(&path)
        .expect("launcher")
        .replace("framework_controls:v1", "unserved_controls:v1");
    fs::write(&path, text).expect("launcher");
    let error = refusal(&dirs, path);
    assert!(
        error.contains("would be refused at launch")
            && error.contains(
                "target `stack`: daemon interface `stack_copies:v2` is not one this peppy \
                 serves; it serves `stack_copies` at tag `v1` only"
            ),
        "{error}"
    );
}
