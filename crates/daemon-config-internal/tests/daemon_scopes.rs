//! `daemon_scopes` and `set_daemon_scopes`: how a launcher scopes the daemon
//! targets of its exposure deployments, the refusal of a scope written
//! through a copy, and the launch checks every scope meets once the
//! exposure deployments are resolved.

use config::runtime::Name;
use core_node_api::encoding::LaunchJoin;
use daemon_config::daemon_interface::DaemonInterface;
use daemon_config::launcher::{
    AppliedChange, ComposedLaunch, CompositionError, CopyMembership, DaemonScopeError,
    DaemonScopeRefusals, Deployment, DeploymentInstance, DeploymentSource, JoinRequest,
    PeppyLauncher, PeppyLauncherParser, PreparedLauncher, RunningStack, SkipReason,
    check_daemon_scopes,
};
use daemon_config::mcp_deployment::DeploymentTargets;
use serde_json::json;
use std::collections::BTreeSet;
use std::path::Path;

mod common;
use common::words;

/// The scope `simulation_mcp` gives its framework endpoint.
const SCOPE: &str = r#"{
    max_copies: 4,
    options: [
        { option: "openarm_sim", description: "A simulated OpenArm v2 standing on the floor" },
        { option: "so101_sim", description: "A simulated SO-101 clamped on the edge of a table" },
    ],
}"#;

/// The shape of `simulation_mcp`: the `robot_control` option deploys the
/// framework's endpoint, a fixed surface whose one target is a daemon
/// target; a top-level adjustment scopes it; the `robot` axis runs as
/// copies. `extra_deployments`, `extra_adjustments` and
/// `extra_robot_options` add to the launcher's lists.
fn simulation_mcp_with(
    extra_deployments: &str,
    extra_adjustments: &str,
    extra_robot_options: &str,
) -> String {
    format!(
        r#"{{
        peppy_schema: "launcher/v1",
        components: [
            {{ name: "robot_control", cardinality: "one", options: {{
                robot_control: {{ deployments: [
                    {{ source: {{ exposures: ["framework_controls:v1"] }}, instances: [
                        {{ instance_id: "framework_controls_inst", arguments: {{ port: 8903 }} }}
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
                {extra_robot_options}
            }} }},
        ],
        deployments: [
            {{ robot_control: "robot_control" }},
            {extra_deployments}
        ],
        adjustments: [
            {{ target: "framework_controls_inst", set_daemon_scopes: {{ stack: {SCOPE} }} }},
            {extra_adjustments}
        ],
    }}"#
    )
}

fn simulation_mcp(extra_adjustments: &str, extra_robot_options: &str) -> String {
    simulation_mcp_with("", extra_adjustments, extra_robot_options)
}

fn load(document: &str) -> PreparedLauncher {
    let parsed = PeppyLauncherParser::from_content(document).expect("the launcher parses");
    PreparedLauncher::load(&parsed, Path::new("simulation_mcp.json5")).expect("it loads")
}

fn name(value: &str) -> Name {
    Name::new(value).expect("a name")
}

fn launch_join(option: &str, copy: &str) -> LaunchJoin {
    LaunchJoin {
        option: option.to_owned(),
        name: name(copy),
    }
}

fn instance<'a>(flat: &'a PeppyLauncher, id: &str) -> &'a DeploymentInstance {
    flat.deployments
        .iter()
        .flat_map(|deployment| &deployment.instances)
        .find(|instance| instance.instance_id.as_str() == id)
        .unwrap_or_else(|| panic!("{id} is in the plan"))
}

/// The targets the resolution of each deployment would report: the
/// framework's endpoint serves the daemon target `stack`, the camera
/// endpoint the contract target `front_camera`, and a node none.
fn targets_of(deployment: &Deployment) -> DeploymentTargets {
    let DeploymentSource::Exposures { exposures } = &deployment.source else {
        return DeploymentTargets::default();
    };
    let mut targets = DeploymentTargets::default();
    for exposure in exposures {
        match exposure.name.as_str() {
            "framework_controls" => {
                targets
                    .daemon
                    .insert("stack".to_owned(), DaemonInterface::StackCopies);
            }
            "camera" => {
                targets.contract.insert("front_camera".to_owned());
            }
            other => panic!("no targets known for exposure `{other}`"),
        }
    }
    targets
}

fn check(
    prepared: &PreparedLauncher,
    flat: &PeppyLauncher,
    copies: &CopyMembership,
) -> Result<(), DaemonScopeRefusals> {
    let targets: Vec<DeploymentTargets> = flat.deployments.iter().map(targets_of).collect();
    check_daemon_scopes(prepared, flat.deployments.iter().zip(&targets), copies)
}

fn refusals(prepared: &PreparedLauncher, flat: &PeppyLauncher) -> Vec<DaemonScopeError> {
    check(prepared, flat, &CopyMembership::default())
        .expect_err("the scopes are refused")
        .0
}

/// The shape of `simulation_mcp`: the top-level scope runs in the stack and
/// in no copy, so a launch with `--join` and a later join both compose, and
/// the scopes hold.
#[test]
fn a_top_level_scope_runs_in_the_stack_and_copies_join_beside_it() {
    let prepared = load(&simulation_mcp("", ""));
    let ComposedLaunch {
        launcher: flat,
        selection,
        report,
    } = prepared
        .launch(&[], &[launch_join("openarm_sim", "alpha")])
        .expect("a launch with a joined robot composes");
    let scoped = instance(&flat, "framework_controls_inst");
    let expected: serde_json::Value = serde_json5::from_str(SCOPE).expect("the scope parses");
    assert_eq!(scoped.daemon_scopes["stack"], expected);
    let scope_writes: Vec<(&str, &str)> = report
        .applied
        .iter()
        .filter(|entry| matches!(entry.change, AppliedChange::DaemonScope { .. }))
        .map(|entry| (entry.target.as_str(), entry.origin.as_str()))
        .collect();
    assert_eq!(
        scope_writes,
        [(
            "framework_controls_inst",
            "simulation_mcp.json5, top-level `adjustments`"
        )],
        "the stack applies the scope once, and the copy does not"
    );
    let rendered = report.render_lines().join("\n");
    assert!(
        rendered.contains("framework_controls_inst.daemon_scopes.stack: (absent) -> {"),
        "{rendered}"
    );

    let copies = CopyMembership::of(report.copies.iter());
    check(&prepared, &flat, &copies).expect("the scopes hold at launch");

    let joined = prepared
        .join(
            JoinRequest {
                option: "so101_sim",
                name: &name("bravo"),
                words: &[],
                arguments: &[],
            },
            RunningStack {
                selection: &selection,
                launcher: &flat,
            },
        )
        .expect("a later join composes beside the scoped instance");
    assert!(
        joined
            .report
            .applied
            .iter()
            .all(|entry| !matches!(entry.change, AppliedChange::DaemonScope { .. })),
        "a join applies no scope"
    );
    assert_eq!(
        instance(&joined.launcher, "framework_controls_inst").daemon_scopes,
        scoped.daemon_scopes
    );
    let copies = CopyMembership::of(report.copies.iter().chain([&joined.copy]));
    check(&prepared, &joined.launcher, &copies).expect("the scopes hold after the join");

    // The same launcher, checked as `repo index --check` checks it.
    let parsed = PeppyLauncherParser::from_content(&simulation_mcp("", "")).expect("parses");
    assert_eq!(
        daemon_config::launcher::check_composition(&parsed, Path::new("simulation_mcp.json5")),
        Vec::<String>::new()
    );
}

/// Under `robot_control=none` the instance is not deployed, so the
/// adjustment that scopes it is skipped, as every adjustment whose target
/// the selection does not define is.
#[test]
fn a_scope_of_an_instance_the_selection_does_not_define_is_skipped() {
    let prepared = load(&simulation_mcp("", ""));
    let composed = prepared
        .launch(&words(&["robot_control=none"]), &[])
        .expect("composes");
    assert!(composed.report.skipped.iter().any(|entry| {
        entry.target == "framework_controls_inst"
            && matches!(entry.reason, SkipReason::TargetAbsent)
    }));
    check(&prepared, &composed.launcher, &CopyMembership::default())
        .expect("no instance serves a daemon target");
}

#[test]
fn set_daemon_scopes_replaces_the_scope_of_each_named_target() {
    let prepared = load(&simulation_mcp(
        r#"{ target: "framework_controls_inst",
             set_daemon_scopes: { stack: { max_copies: 1, options: [
                 { option: "so101_sim", description: "One SO-101" } ] } } },"#,
        "",
    ));
    let composed = prepared.launch(&[], &[]).expect("composes");
    assert_eq!(
        instance(&composed.launcher, "framework_controls_inst").daemon_scopes["stack"],
        json!({ "max_copies": 1, "options": [{ "option": "so101_sim", "description": "One SO-101" }] }),
        "the later adjustment wins"
    );
    let changes: Vec<(Option<serde_json::Value>, serde_json::Value)> = composed
        .report
        .applied
        .iter()
        .filter_map(|entry| match &entry.change {
            AppliedChange::DaemonScope { old, new, .. } => Some((old.clone(), new.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0].0, None, "the first adjustment creates the scope");
    assert_eq!(
        changes[1].0.as_ref(),
        Some(&changes[0].1),
        "the second replaces it"
    );
}

#[test]
fn an_adjustment_with_an_empty_set_daemon_scopes_names_no_operation() {
    let error = PeppyLauncherParser::from_content(&simulation_mcp(
        r#"{ target: "framework_controls_inst", set_daemon_scopes: {} },"#,
        "",
    ))
    .expect_err("an empty map writes nothing");
    let message = error.to_string();
    assert!(
        message.contains("names no operation")
            && message.contains("`unset_links`, `set_daemon_scopes`"),
        "{message}"
    );
}

/// The refusal of a scope written through a copy of `option`, which a
/// launch-time `--join`, a later join and the repository check all compose
/// through.
fn assert_copy_scope_refusal(error: &CompositionError, copy: &str, option: &str, instance: &str) {
    let CompositionError::CopySetsDaemonScope {
        copy: refused_copy,
        option: refused_option,
        instance: refused_instance,
        daemon_target,
        ..
    } = error
    else {
        panic!("expected the copy scope refusal, got {error}");
    };
    assert_eq!(refused_copy, copy);
    assert_eq!(refused_option, option);
    assert_eq!(refused_instance, instance);
    assert_eq!(daemon_target, "stack");
    let message = error.to_string();
    assert!(
        message.contains("a scope belongs to the stack"),
        "{message}"
    );
    // The launchers hub's CI reads a join refusal that says "would change"
    // as an expected refusal of a running instance; this one is not that.
    assert!(!message.contains("would change"), "{message}");
}

#[test]
fn a_scope_written_through_a_copy_is_refused() {
    // A top-level adjustment guarded by an option of the copy axis runs in
    // each copy of that option.
    let guarded = simulation_mcp(
        r#"{ target: "framework_controls_inst", when: { robot: "openarm_sim" },
             set_daemon_scopes: { stack: { max_copies: 1, options: [
                 { option: "openarm_sim", description: "One OpenArm" } ] } } },"#,
        "",
    );
    let prepared = load(&guarded);
    let error = prepared
        .launch(&[], &[launch_join("openarm_sim", "alpha")])
        .expect_err("the copy writes a scope");
    assert_copy_scope_refusal(&error, "alpha", "openarm_sim", "framework_controls_inst");

    let bare = prepared.launch(&[], &[]).expect("the stack alone composes");
    let error = prepared
        .join(
            JoinRequest {
                option: "openarm_sim",
                name: &name("bravo"),
                words: &[],
                arguments: &[],
            },
            RunningStack {
                selection: &bare.selection,
                launcher: &bare.launcher,
            },
        )
        .expect_err("a join that writes a scope is refused");
    assert_copy_scope_refusal(&error, "bravo", "openarm_sim", "framework_controls_inst");

    let parsed = PeppyLauncherParser::from_content(&guarded).expect("parses");
    let problems =
        daemon_config::launcher::check_composition(&parsed, Path::new("simulation_mcp.json5"));
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("a scope belongs to the stack")),
        "{problems:?}"
    );
}

#[test]
fn a_scope_written_under_an_option_that_runs_as_copies_is_refused() {
    // The adjustments of an option of the copy axis run in each copy of that
    // option, here on the stack's endpoint.
    let document = simulation_mcp(
        "",
        r#"scoping_sim: {
            deployments: [
                { source: { name: "so101", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] }
            ],
            adjustments: [
                { target: "framework_controls_inst",
                  set_daemon_scopes: { stack: { max_copies: 1, options: [
                      { option: "scoping_sim", description: "One SO-101" } ] } } }
            ],
        },"#,
    );
    let prepared = load(&document);
    let error = prepared
        .launch(&[], &[launch_join("scoping_sim", "alpha")])
        .expect_err("the copy writes a scope");
    assert_copy_scope_refusal(&error, "alpha", "scoping_sim", "framework_controls_inst");

    let bare = prepared.launch(&[], &[]).expect("the stack alone composes");
    let error = prepared
        .join(
            JoinRequest {
                option: "scoping_sim",
                name: &name("bravo"),
                words: &[],
                arguments: &[],
            },
            RunningStack {
                selection: &bare.selection,
                launcher: &bare.launcher,
            },
        )
        .expect_err("a join that writes a scope is refused");
    assert_copy_scope_refusal(&error, "bravo", "scoping_sim", "framework_controls_inst");

    let parsed = PeppyLauncherParser::from_content(&document).expect("parses");
    let problems =
        daemon_config::launcher::check_composition(&parsed, Path::new("simulation_mcp.json5"));
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("a scope belongs to the stack")),
        "{problems:?}"
    );
}

#[test]
fn a_copy_option_that_scopes_its_own_instance_is_refused() {
    // A launcher entry of a copy, with the copy's own adjustment on the
    // instance its option deploys.
    let document = simulation_mcp_with(
        r#"{ robot: "openarm_sim", instances: [
                { instance_id: "alpha", adjustments: [
                    { target: "arm_inst", set_daemon_scopes: { stack: { max_copies: 1 } } }
                ] }
            ] },"#,
        "",
        "",
    );
    let prepared = load(&document);
    let error = prepared
        .launch(&[], &[])
        .expect_err("the copy writes a scope");
    assert_copy_scope_refusal(&error, "alpha", "openarm_sim", "alpha_arm_inst");
}

#[test]
fn a_copy_entry_declares_no_daemon_scopes() {
    let document = simulation_mcp_with(
        r#"{ robot: "openarm_sim", instances: [
                { instance_id: "alpha", daemon_scopes: { stack: {} } }
            ] },"#,
        "",
        "",
    );
    let error = PeppyLauncherParser::from_content(&document).expect_err("refused");
    assert!(
        error
            .to_string()
            .contains("copy `alpha` declares `daemon_scopes`; a scope belongs to the stack"),
        "{error}"
    );
}

/// A flat launcher (no axes) whose deployments are written in `deployments`.
fn flat(deployments: &str) -> (PreparedLauncher, PeppyLauncher) {
    let document = format!(
        r#"{{
        peppy_schema: "launcher/v1",
        core_nodes: ["robot_pc"],
        deployments: [{deployments}],
    }}"#
    );
    let prepared = load(&document);
    let composed = prepared.launch(&[], &[]).expect("composes");
    (prepared, composed.launcher)
}

const FRAMEWORK: &str = r#"{ source: { exposures: ["framework_controls:v1"] }, instances: ["#;

#[test]
fn an_instance_that_serves_no_daemon_target_takes_no_scope() {
    let (prepared, launcher) = flat(&format!(
        r#"{{ source: {{ name: "arm", tag: "v1" }}, instances: [
               {{ instance_id: "arm_inst", daemon_scopes: {{ stack: {SCOPE} }} }} ] }},
           {{ source: {{ exposures: ["camera:v1"] }}, instances: [
               {{ instance_id: "camera_inst", daemon_scopes: {{ stack: {SCOPE} }} }} ] }}"#
    ));
    let refused = refusals(&prepared, &launcher);
    assert_eq!(
        refused,
        [
            DaemonScopeError::ScopesWithoutDaemonTarget {
                instance: "arm_inst".to_owned()
            },
            DaemonScopeError::ScopesWithoutDaemonTarget {
                instance: "camera_inst".to_owned()
            },
        ]
    );
    assert!(
        refused[0].to_string().starts_with(
            "instance `arm_inst` declares `daemon_scopes`, but it serves no daemon target"
        ),
        "{}",
        refused[0]
    );
}

#[test]
fn a_scope_key_names_a_daemon_target_of_the_instance() {
    let (prepared, launcher) = flat(&format!(
        r#"{{ source: {{ exposures: ["framework_controls:v1", "camera:v1"] }}, instances: [
               {{ instance_id: "mcp_inst", links: {{ front_camera: "camera_inst" }},
                  daemon_scopes: {{ stack: {SCOPE}, front_camera: {{}}, stacks: {{}} }} }} ] }},
           {{ source: {{ name: "camera", tag: "v1" }}, instances: [
               {{ instance_id: "camera_inst" }} ] }}"#
    ));
    let messages: Vec<String> = refusals(&prepared, &launcher)
        .iter()
        .filter(|refusal| matches!(refusal, DaemonScopeError::UnknownScopeKey { .. }))
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        messages,
        [
            "`daemon_scopes.front_camera` of instance `mcp_inst` names no daemon target of its \
             exposures (`front_camera` is a contract target, which takes `links`); its daemon \
             targets: `stack`",
            "`daemon_scopes.stacks` of instance `mcp_inst` names no daemon target of its \
             exposures; its daemon targets: `stack`",
        ]
    );
}

#[test]
fn every_daemon_target_takes_a_scope() {
    let (prepared, launcher) = flat(&format!(
        r#"{FRAMEWORK} {{ instance_id: "mcp_inst" }} ] }}"#
    ));
    let refused = refusals(&prepared, &launcher);
    assert_eq!(
        refused,
        [DaemonScopeError::MissingScope {
            instance: "mcp_inst".to_owned(),
            daemon_target: "stack".to_owned(),
            interface: "stack_copies:v1".to_owned(),
        }]
    );
    let message = refused[0].to_string();
    assert!(
        message.contains("has no scope")
            && message.contains("`daemon_scopes`")
            && message.contains("`set_daemon_scopes` adjustment on `mcp_inst`"),
        "{message}"
    );
}

#[test]
fn a_scope_parses_into_its_interfaces_type() {
    let (prepared, launcher) = flat(&format!(
        r#"{FRAMEWORK} {{ instance_id: "mcp_inst", daemon_scopes: {{ stack: {{
               max_copies: 0, options: [{{ option: "a", description: "One." }}] }} }} }} ] }}"#
    ));
    let refused = refusals(&prepared, &launcher);
    assert_eq!(refused.len(), 1, "{refused:?}");
    let message = refused[0].to_string();
    assert!(
        message.starts_with(
            "the scope of daemon target `stack` (stack_copies:v1) on instance `mcp_inst` does not \
             parse:"
        ) && message.contains("`max_copies` is a whole number from 1 to 65535, got 0"),
        "{message}"
    );
}

#[test]
fn a_stack_copies_scope_names_options_of_an_axis_that_runs_as_copies() {
    let prepared = load(&simulation_mcp(
        r#"{ target: "framework_controls_inst",
             set_daemon_scopes: { stack: { max_copies: 4, options: [
                 { option: "openarm_sim", description: "An OpenArm" },
                 { option: "robot_control", description: "Not a robot" } ] } } },"#,
        "",
    ));
    let composed = prepared.launch(&[], &[]).expect("composes");
    let refused = refusals(&prepared, &composed.launcher);
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(
        refused[0].to_string(),
        "the scope of daemon target `stack` (stack_copies:v1) on instance \
         `framework_controls_inst` does not hold against the launcher: `robot_control` is not an \
         option of an axis this launcher runs as copies; the copies it can add:\n  - robot: \
         `openarm_sim`, `so101_sim`"
    );

    // A launcher with no axis that runs as copies takes no copy at all: one
    // refusal says why, whatever the scope names, and how a flat launcher
    // with a scope launches.
    let (prepared, launcher) = flat(&format!(
        r#"{FRAMEWORK} {{ instance_id: "mcp_inst", daemon_scopes: {{ stack: {SCOPE} }} }} ] }}"#
    ));
    let messages: Vec<String> = refusals(&prepared, &launcher)
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        messages,
        [
            "the scope of daemon target `stack` (stack_copies:v1) on instance `mcp_inst` does not \
          hold against the launcher: this launcher declares no axis that runs as copies \
          (`zero_or_more` or `one_or_more`), so no option of a `stack_copies` scope can be \
          added; a flat launcher, such as the one `peppy stack resolve` prints, declares no \
          axis at all: launch the launcher it was resolved from, or take the instance that \
          serves this target out of the flat file"
        ]
    );
}

#[test]
fn an_instance_that_serves_a_daemon_target_runs_on_the_coordinator() {
    let (prepared, launcher) = flat(&format!(
        r#"{FRAMEWORK} {{ instance_id: "mcp_inst", core_node: "robot_pc",
               daemon_scopes: {{ stack: {SCOPE} }} }} ] }}"#
    ));
    // The scope's own options are checked too; this flat launcher has no
    // copy axis, so keep to the placement refusal.
    let refused = refusals(&prepared, &launcher);
    assert_eq!(
        refused[0],
        DaemonScopeError::PlacedOffCoordinator {
            instance: "mcp_inst".to_owned(),
            daemon_target: "stack".to_owned(),
            core_node: "robot_pc".to_owned(),
        }
    );
    assert!(
        refused[0]
            .to_string()
            .contains("runs on the coordinator, the daemon whose stack it reads and changes"),
        "{}",
        refused[0]
    );

    // An option of the copy axis that deploys the endpoint: each copy's
    // instance would run on the copy's placement.
    let prepared = load(&simulation_mcp(
        "",
        r#"endpoint: { deployments: [
            { source: { exposures: ["framework_controls:v1"] }, instances: [
                { instance_id: "endpoint_inst" } ] } ] },"#,
    ));
    let composed = prepared
        .launch(&[], &[launch_join("endpoint", "delta")])
        .expect("composes");
    let copies = CopyMembership::of(composed.copies());
    let refused = check(&prepared, &composed.launcher, &copies)
        .expect_err("the copy's instance serves a daemon target")
        .0;
    assert!(
        refused.contains(&DaemonScopeError::InCopy {
            instance: "delta_endpoint_inst".to_owned(),
            copy: "delta".to_owned(),
            daemon_target: "stack".to_owned(),
        }),
        "{refused:?}"
    );
    assert!(
        refused.contains(&DaemonScopeError::MissingScope {
            instance: "delta_endpoint_inst".to_owned(),
            daemon_target: "stack".to_owned(),
            interface: "stack_copies:v1".to_owned(),
        }),
        "{refused:?}"
    );
}

#[test]
fn a_daemon_target_takes_no_link() {
    let prepared = load(&simulation_mcp("", ""));
    let document = simulation_mcp(
        r#"{ target: "framework_controls_inst", set_links: { stack: "framework_controls_inst" } },"#,
        "",
    );
    let linked = load(&document);
    let composed = linked.launch(&[], &[]).expect("composes");
    let refused = refusals(&prepared, &composed.launcher);
    assert_eq!(
        refused,
        [DaemonScopeError::LinkToDaemonTarget {
            instance: "framework_controls_inst".to_owned(),
            daemon_target: "stack".to_owned(),
        }]
    );
    assert!(
        refused[0]
            .to_string()
            .contains("which takes no link: the daemon that started the server serves it, and the target takes a scope instead"),
        "{}",
        refused[0]
    );
}

#[test]
fn every_refusal_is_reported_at_once() {
    let (prepared, launcher) = flat(&format!(
        r#"{FRAMEWORK} {{ instance_id: "mcp_inst", core_node: "robot_pc" }} ] }},
           {{ source: {{ name: "arm", tag: "v1" }}, instances: [
               {{ instance_id: "arm_inst", daemon_scopes: {{ stack: {{}} }} }} ] }}"#
    ));
    let error = check(&prepared, &launcher, &CopyMembership::default()).expect_err("refused");
    assert_eq!(error.0.len(), 3, "{error}");
    let rendered = error.to_string();
    assert!(
        rendered.starts_with("the launcher's daemon scopes do not hold:\n  - "),
        "{rendered}"
    );
    let instances: BTreeSet<&str> = ["mcp_inst", "arm_inst"].into_iter().collect();
    assert!(
        instances.iter().all(|id| rendered.contains(id)),
        "{rendered}"
    );
}
