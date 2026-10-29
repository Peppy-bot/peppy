//! The rules a launcher's restart policy is held to under composition:
//! `framework.restart` at a root is the launcher root's alone, an adjustment
//! writes an instance's own, and copies agree on what they write.

mod common;

use common::{copy_name, fragment_file, name, words, write};
use config::node::{RestartDeclaration, RestartPolicy};
use daemon_config::launcher::{
    AppliedChange, CompositionError, JoinRequest, PeppyLauncherParser, PreparedLauncher,
    RunningStack,
};
use std::num::NonZeroU32;
use std::path::Path;
use tempfile::tempdir;

/// A launcher on disk, held ready to compose as `stack launch` holds it.
fn prepare(directory: &Path, document: &str) -> PreparedLauncher {
    let file = write(&directory.join("fleet.json5"), document);
    let parsed = PeppyLauncherParser::from_content(document).expect("the launcher parses");
    PreparedLauncher::load(&parsed, &file).expect("the fragments load")
}

fn on_failure() -> RestartDeclaration {
    RestartDeclaration::policy_only(RestartPolicy::OnFailure)
}

fn capped(policy: RestartPolicy, max_restarts: u32) -> RestartDeclaration {
    RestartDeclaration::new(policy, NonZeroU32::new(max_restarts)).expect("a cap on a restart")
}

/// The restart entry an adjustment wrote on `target`, with the origin
/// the report names.
fn restart_change(
    report: &daemon_config::launcher::CompositionReport,
    target: &str,
) -> (Option<RestartDeclaration>, RestartDeclaration, String) {
    let entry = report
        .applied
        .iter()
        .find(|entry| entry.target == target && entry.change.field() == "framework.restart")
        .unwrap_or_else(|| panic!("an adjustment writes `{target}`'s restart"));
    let AppliedChange::Restart { old, new } = &entry.change else {
        panic!("expected a Restart change, got {:?}", entry.change);
    };
    (*old, *new, entry.origin.clone())
}

/// A fragment composes under the root that selects it, so a policy at its
/// root is refused naming the fragment and where the policy belongs.
#[test]
fn fragment_root_restart_is_refused() {
    let directory = tempdir().unwrap();
    write(
        &directory.path().join("arm.json5"),
        &fragment_file(
            r#"framework: { restart: "on_failure" },
               deployments: [{ source: { name: "arm", tag: "v1" },
                               instances: [{ instance_id: "arm_inst" }] }]"#,
        ),
    );
    let launcher = r#"{
        peppy_schema: "launcher/v1",
        description: ["A test launcher."],
        components: [
            { name: "robot", cardinality: "zero_or_one", options: { real: "arm.json5" } },
        ],
        deployments: [
            { source: { name: "panel", tag: "v1" }, instances: [{ instance_id: "panel_inst" }] },
        ],
    }"#;
    let prepared = prepare(directory.path(), launcher);
    let error = prepared
        .launch(&words(&["real"]), &[])
        .expect_err("a fragment cannot write `framework.restart`");
    assert_refuses_a_fragment_root_restart(&error, "arm.json5");
    // The same launcher composes once the fragment is left out.
    prepared
        .launch(&[], &[])
        .expect("an unselected fragment is never read for its policy");
}

/// Asserts that `error` refuses a fragment's root `framework.restart` from an
/// origin naming `named`, with the whole text that says where the policy
/// belongs.
fn assert_refuses_a_fragment_root_restart(error: &CompositionError, named: &str) {
    let CompositionError::FragmentDeclaresRestart { origin } = error else {
        panic!("expected FragmentDeclaresRestart, got: {error}");
    };
    assert!(origin.contains(named), "{origin}");
    assert_eq!(
        error.to_string(),
        format!(
            "{origin} writes `framework.restart` at its root, which only the launcher's own \
             root takes; write `framework: {{ restart: ... }}` on its instance entries, or move \
             it to the launcher root"
        )
    );
}

/// An inline option body is a fragment, so a policy at its root is refused
/// the same way, naming the option.
#[test]
fn option_root_restart_is_refused() {
    let directory = tempdir().unwrap();
    let launcher = r#"{
        peppy_schema: "launcher/v1",
        description: ["A test launcher."],
        components: [
            { name: "robot", cardinality: "zero_or_one", options: { real: {
                framework: { restart: "always" },
                deployments: [{ source: { name: "arm", tag: "v1" },
                                instances: [{ instance_id: "arm_inst" }] }],
            } } },
        ],
        deployments: [
            { source: { name: "panel", tag: "v1" }, instances: [{ instance_id: "panel_inst" }] },
        ],
    }"#;
    let error = prepare(directory.path(), launcher)
        .launch(&words(&["real"]), &[])
        .expect_err("an option body cannot write `framework.restart`");
    assert_refuses_a_fragment_root_restart(&error, "robot.real");
}

/// A copy's fragments are held to the same rule when the copy composes.
#[test]
fn copy_fragment_root_restart_is_refused() {
    let directory = tempdir().unwrap();
    write(
        &directory.path().join("arm.json5"),
        &fragment_file(
            r#"framework: { restart: "on_failure" },
               deployments: [{ source: { name: "arm", tag: "v1" },
                               instances: [{ instance_id: "arm_inst" }] }]"#,
        ),
    );
    let launcher = r#"{
        peppy_schema: "launcher/v1",
        description: ["A test launcher."],
        components: [
            { name: "robot", cardinality: "zero_or_more", options: { real: "arm.json5" } },
        ],
        deployments: [
            { source: { name: "panel", tag: "v1" }, instances: [{ instance_id: "panel_inst" }] },
            { robot: "real", instances: [{ instance_id: "alpha" }] },
        ],
    }"#;
    let error = prepare(directory.path(), launcher)
        .launch(&[], &[])
        .expect_err("a copy's fragment cannot write `framework.restart`");
    assert_refuses_a_fragment_root_restart(&error, "arm.json5");
}

/// A copy composes its fragment on its own at join, so a policy at the
/// fragment's root is refused there too, naming the fragment.
#[test]
fn joining_a_copy_whose_fragment_writes_the_restart_is_refused() {
    let directory = tempdir().unwrap();
    write(
        &directory.path().join("arm.json5"),
        &fragment_file(
            r#"framework: { restart: "on_failure" },
               deployments: [{ source: { name: "arm", tag: "v1" },
                               instances: [{ instance_id: "arm_inst" }] }]"#,
        ),
    );
    let launcher = r#"{
        peppy_schema: "launcher/v1",
        description: ["A test launcher."],
        components: [
            { name: "robot", cardinality: "zero_or_more", options: { real: "arm.json5" } },
        ],
        deployments: [
            { source: { name: "panel", tag: "v1" }, instances: [{ instance_id: "panel_inst" }] },
        ],
    }"#;
    let prepared = prepare(directory.path(), launcher);
    let launch = prepared
        .launch(&[], &[])
        .expect("a launch that selects no copy");
    let error = prepared
        .join(
            JoinRequest {
                option: &name("real"),
                name: &copy_name("alpha"),
                words: &[],
                arguments: &[],
            },
            RunningStack {
                selection: &launch.selection,
                launcher: &launch.launcher,
            },
        )
        .expect_err("the copy's fragment cannot write `framework.restart`");
    assert_refuses_a_fragment_root_restart(&error, "arm.json5");
}

/// The flat document a launch runs carries the launcher root's policy
/// through the merge of the selected fragments' frameworks.
#[test]
fn merge_framework_keeps_the_root_restart() {
    let directory = tempdir().unwrap();
    write(
        &directory.path().join("sim.json5"),
        &fragment_file(
            r#"framework: { clocks: { simulation: { publisher: "engine_inst" } } },
               deployments: [{ source: { name: "engine", tag: "v1" },
                               instances: [{ instance_id: "engine_inst" }] }]"#,
        ),
    );
    let launcher = r#"{
        peppy_schema: "launcher/v1",
        description: ["A test launcher."],
        framework: { restart: { policy: "on_failure", max_restarts: 5 } },
        components: [
            { name: "simulation", cardinality: "zero_or_one", options: { mujoco: "sim.json5" } },
        ],
        deployments: [
            { source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] },
        ],
    }"#;
    let composed = prepare(directory.path(), launcher)
        .launch(&words(&["mujoco"]), &[])
        .expect("a fragment's clocks merge beside the root's policy");
    assert_eq!(
        composed.launcher.framework.restart,
        Some(capped(RestartPolicy::OnFailure, 5))
    );
    assert!(
        composed
            .launcher
            .framework
            .clocks
            .keys()
            .any(|domain| domain.as_str() == "simulation"),
        "the fragment's domain is merged"
    );
}

/// A launcher refuses a bad restart value at its root and on an instance
/// entry with the text the key's rule gives.
#[test]
fn a_launcher_refuses_a_bad_restart_value() {
    let refusal = |root: &str, entry: &str| {
        PeppyLauncherParser::from_content(&format!(
            r#"{{
                peppy_schema: "launcher/v1",
                description: ["A test launcher."],
                {root}
                deployments: [
                    {{ source: {{ name: "arm", tag: "v1" }}, instances: [{{ instance_id: "arm_inst"{entry} }}] }},
                ],
            }}"#
        ))
        .expect_err("the launcher is refused")
        .to_string()
    };
    let sometimes = "`restart` is `never`, `on_failure` or `always`; got `sometimes`";
    let err = refusal(r#"framework: { restart: "sometimes" },"#, "");
    assert!(err.contains(sometimes), "{err}");
    let err = refusal("", r#", framework: { restart: "sometimes" }"#);
    assert!(err.contains(sometimes), "{err}");
    let err = refusal("framework: { restart: { max_restarts: 2 } },", "");
    assert!(
        err.contains("`restart` as an object needs `policy`"),
        "{err}"
    );
    let err = refusal(
        "",
        r#", framework: { restart: { policy: "never", max_restarts: 2 } }"#,
    );
    assert!(
        err.contains("`max_restarts` takes a policy that restarts"),
        "{err}"
    );
}

/// `set_framework: { restart }` alone is an operation, so an adjustment
/// carrying nothing else parses.
#[test]
fn set_framework_restart_counts_as_an_operation() {
    let launcher = r#"{
        peppy_schema: "launcher/v1",
        description: ["A test launcher."],
        components: [
            { name: "mode", cardinality: "zero_or_one", options: { field: {
                adjustments: [{ target: "arm_inst", set_framework: { restart: "always" } }],
            } } },
        ],
        deployments: [
            { source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] },
        ],
    }"#;
    PeppyLauncherParser::from_content(launcher).expect("a restart adjustment is an operation");
}

/// Adjustments write the policy in order, each recording the value it
/// replaced.
#[test]
fn set_framework_restart_applies_in_adjustment_order() {
    let directory = tempdir().unwrap();
    let launcher = r#"{
        peppy_schema: "launcher/v1",
        description: ["A test launcher."],
        components: [
            { name: "mode", cardinality: "zero_or_one", options: { field: {
                adjustments: [{ target: "arm_inst", set_framework: { restart: "on_failure" } }],
            } } },
        ],
        adjustments: [
            { target: "arm_inst", set_framework: { restart: { policy: "always", max_restarts: 3 } } },
        ],
        deployments: [
            { source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] },
        ],
    }"#;
    let composed = prepare(directory.path(), launcher)
        .launch(&words(&["field"]), &[])
        .expect("both adjustments apply");
    let arm = &composed.launcher.deployments[0].instances[0];
    assert_eq!(
        arm.framework.restart,
        Some(capped(RestartPolicy::Always, 3))
    );
    let writes: Vec<(Option<RestartDeclaration>, RestartDeclaration)> = composed
        .report
        .applied
        .iter()
        .filter(|entry| entry.change.field() == "framework.restart")
        .map(|entry| match &entry.change {
            AppliedChange::Restart { old, new } => (*old, *new),
            other => panic!("expected a Restart change, got {other:?}"),
        })
        .collect();
    assert_eq!(
        writes,
        [
            (None, on_failure()),
            (Some(on_failure()), capped(RestartPolicy::Always, 3)),
        ]
    );
    let lines = composed.report.render_lines();
    assert!(
        lines
            .iter()
            .any(|line| line.contains(r#"arm_inst.framework.restart: (absent) -> "on_failure""#)),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.contains(
            r#"arm_inst.framework.restart: "on_failure" -> { policy: "always", max_restarts: 3 }"#
        )),
        "{lines:?}"
    );
}

/// The recorded change names the adjustment's origin beside the field, and
/// the report renders it after the change.
#[test]
fn applied_change_restart_renders_with_origin() {
    let directory = tempdir().unwrap();
    let launcher = r#"{
        peppy_schema: "launcher/v1",
        description: ["A test launcher."],
        components: [
            { name: "mode", cardinality: "zero_or_one", options: { field: {
                adjustments: [{ target: "arm_inst", set_framework: { restart: "never" } }],
            } } },
        ],
        deployments: [
            { source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] },
        ],
    }"#;
    let composed = prepare(directory.path(), launcher)
        .launch(&words(&["field"]), &[])
        .expect("the adjustment applies");
    let (old, new, origin) = restart_change(&composed.report, "arm_inst");
    assert_eq!(old, None);
    assert_eq!(new, RestartDeclaration::policy_only(RestartPolicy::Never));
    assert_eq!(origin, "fleet.json5, option `mode.field`");
    let lines = composed.report.render_lines();
    assert!(
        lines.contains(&r#"  arm_inst.framework.restart: (absent) -> "never"  (fleet.json5, option `mode.field`)"#.to_owned()),
        "{lines:?}"
    );
}

/// Two copies of one option writing the same policy on a stack instance
/// agree; two options writing different policies are refused naming the
/// field, as any disagreeing write is.
#[test]
fn copies_must_agree_on_restart() {
    let directory = tempdir().unwrap();
    let document = |copies: &str| {
        format!(
            r#"{{
            peppy_schema: "launcher/v1",
            description: ["A test launcher."],
            components: [
                {{ name: "simulation", options: {{ engine: {{ deployments: [
                    {{ source: {{ name: "engine", tag: "v1" }}, instances: [
                        {{ instance_id: "engine_inst" }}
                    ] }}
                ] }} }} }},
                {{ name: "robot", cardinality: "zero_or_more", options: {{
                    sturdy: {{ deployments: [{{ source: {{ name: "arm", tag: "v1" }},
                                               instances: [{{ instance_id: "arm_inst" }}] }}],
                               adjustments: [{{ target: "engine_inst",
                                                set_framework: {{ restart: "always" }} }}] }},
                    fragile: {{ deployments: [{{ source: {{ name: "arm", tag: "v1" }},
                                                instances: [{{ instance_id: "arm_inst" }}] }}],
                                adjustments: [{{ target: "engine_inst",
                                                 set_framework: {{ restart: "never" }} }}] }}
                }} }}
            ],
            deployments: [{{ simulation: "engine" }}, {copies}]
        }}"#
        )
    };
    let agreed = prepare(
        directory.path(),
        &document(
            r#"{ robot: "sturdy", instances: [{ instance_id: "alpha" }, { instance_id: "bravo" }] }"#,
        ),
    )
    .launch(&[], &[])
    .expect("two copies writing one policy agree");
    assert_eq!(
        agreed.launcher.deployments[0].instances[0]
            .framework
            .restart,
        Some(RestartDeclaration::policy_only(RestartPolicy::Always))
    );
    let error = prepare(
        directory.path(),
        &document(
            r#"{ robot: "sturdy", instances: [{ instance_id: "alpha" }] },
               { robot: "fragile", instances: [{ instance_id: "bravo" }] }"#,
        ),
    )
    .launch(&[], &[])
    .expect_err("two policies on one instance disagree");
    assert!(
        matches!(&error, CompositionError::CopiesConflict { first, second, target, .. }
            if first == "alpha" && second == "bravo" && target == "engine_inst.framework.restart"),
        "{error}"
    );
}

/// A join writes the running stack's instance only with what it already
/// holds, so a copy whose adjustment changes the policy is refused naming
/// the field and both values.
#[test]
fn join_refuses_a_changed_restart() {
    let directory = tempdir().unwrap();
    let launcher = r#"{
        peppy_schema: "launcher/v1",
        description: ["A test launcher."],
        components: [
            { name: "simulation", options: { engine: { deployments: [
                { source: { name: "engine", tag: "v1" }, instances: [
                    { instance_id: "engine_inst", framework: { restart: "on_failure" } }
                ] }
            ] } } },
            { name: "robot", cardinality: "zero_or_more", options: {
                fragile: { deployments: [{ source: { name: "arm", tag: "v1" },
                                           instances: [{ instance_id: "arm_inst" }] }],
                           adjustments: [{ target: "engine_inst",
                                           set_framework: { restart: { policy: "never" } } }] },
                same: { deployments: [{ source: { name: "arm", tag: "v1" },
                                        instances: [{ instance_id: "arm_inst" }] }],
                        adjustments: [{ target: "engine_inst",
                                        set_framework: { restart: "on_failure" } }] }
            } }
        ],
        deployments: [{ simulation: "engine" }],
    }"#;
    let prepared = prepare(directory.path(), launcher);
    let launch = prepared.launch(&[], &[]).expect("the stack launches");
    let join = |option: &str, copy: &str| {
        prepared.join(
            JoinRequest {
                option: &name(option),
                name: &copy_name(copy),
                words: &[],
                arguments: &[],
            },
            RunningStack {
                selection: &launch.selection,
                launcher: &launch.launcher,
            },
        )
    };
    let error = join("fragile", "alpha").expect_err("the join changes the engine's policy");
    assert!(
        matches!(&error, CompositionError::JoinChangesExisting { instance, changes, .. }
            if instance == "engine_inst"
                && changes == r#"framework.restart: "on_failure" -> "never""#),
        "{error}"
    );
    let joined = join("same", "bravo").expect("a join writing what runs is admitted");
    let (old, new, _) = restart_change(&joined.report, "engine_inst");
    assert_eq!((old, new), (Some(on_failure()), on_failure()));
}
