use config::AnyType;
use daemon_config::launcher::{
    AppliedChange, ComponentAxis, CompositionError, CompositionReport, DeploymentSource,
    FragmentPart, FragmentSpec, LinkValue, PeppyLauncher, PeppyLauncherParser, PreparedLauncher,
    Selection, SkipReason, check_composition,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

mod common;
use common::{fragment_file, words, write};

/// The node name a deployment names; every launcher these tests compose
/// deploys nodes, so an exposure deployment here is a mistake.
fn node_name(deployment: &daemon_config::launcher::Deployment) -> &str {
    match &deployment.source {
        DeploymentSource::Node { name, .. } => name,
        DeploymentSource::Exposures { .. } => panic!("expected a node deployment"),
    }
}

fn parse_launcher(content: &str) -> PeppyLauncher {
    PeppyLauncherParser::from_content(content).expect("launcher parses")
}

/// The flat document and report of a launch of `launcher` under `words`.
fn compose(
    launcher: &PeppyLauncher,
    file: &Path,
    words: &[String],
) -> Result<(PeppyLauncher, CompositionReport), CompositionError> {
    let composed = PreparedLauncher::load(launcher, file)?.launch(words)?;
    Ok((composed.launcher, composed.report))
}

/// The worked micro-example: one base, a robot axis with an inline real
/// option and a file-backed sim option sharing a relay fragment, an
/// optional recorder axis whose attach adjustment reaches whichever
/// commander was selected.
fn composed_fixture(dir: &Path) -> (PathBuf, PeppyLauncher) {
    write(
        &dir.join("fragments/sim_relays.json5"),
        &fragment_file(
            r#"
            deployments: [
                { source: { name: "arm_sim", tag: "v1" },
                  instances: [{ instance_id: "arm_inst", links: { engine: "sim_inst/arm" } }] },
            ],
            adjustments: [
                { target: "backbone_inst", set_arguments: { max_ee_velocity_m_s: 0.5 } },
            ],
        "#,
        ),
    );
    write(
        &dir.join("fragments/mujoco_engine.json5"),
        &fragment_file(
            r#"
            deployments: [
                { source: { name: "sim_mujoco", tag: "v1" },
                  instances: [{ instance_id: "sim_inst", arguments: { state_rate_hz: 100 } }] },
            ],
            adjustments: [
                { target: "recorder_inst",
                  set_arguments: { robot_type: "openarm_mujoco" } },
            ],
        "#,
        ),
    );
    write(
        &dir.join("fragments/recorder.json5"),
        &fragment_file(
            r#"
            deployments: [
                { source: { name: "recorder", tag: "v1" },
                  instances: [{ instance_id: "recorder_inst",
                                links: { observed: ["arm_inst"] } }] },
            ],
            adjustments: [
                { target: "commander_inst", add_links: { recorder: ["recorder_inst"] } },
            ],
        "#,
        ),
    );
    let launcher = write(
        &dir.join("teleop.json5"),
        r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "robot",
                  provides: ["arm_inst"],
                  options: {
                      real: { deployments: [
                          { source: { name: "can_arm", tag: "v1" },
                            instances: [{ instance_id: "arm_inst" }] } ] },
                      mujoco: ["fragments/sim_relays.json5", "fragments/mujoco_engine.json5"],
                  } },
                { name: "recorder",
                  cardinality: "zero_or_one",
                  provides: ["recorder_inst"],
                  options: { on: "fragments/recorder.json5" } },
            ],
            adjustments: [
                { target: "sim_inst", set_arguments: { hardware_version: "v2" } },
            ],
            deployments: [
                { source: { name: "backbone", tag: "v1" },
                  instances: [{ instance_id: "backbone_inst",
                                links: { arm: "arm_inst" } }] },
                { source: { name: "commander", tag: "v1" },
                  instances: [{ instance_id: "commander_inst",
                                links: { backbone: "backbone_inst" } }] },
                { robot: "real" },
            ],
        }"#,
    );
    let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
    (launcher, parsed)
}

#[test]
fn the_file_deploys_the_flat_document() {
    let dir = tempdir().unwrap();
    let (file, launcher) = composed_fixture(dir.path());
    let (flat, report) = compose(&launcher, &file, &[]).expect("composes");

    assert!(flat.components.is_empty());
    assert!(flat.adjustments.is_empty());
    assert!(flat.option_deployments.is_empty());
    // Base deployments first, then the real option's deployment.
    let sources: Vec<String> = flat.deployments.iter().map(|d| d.source.label()).collect();
    assert_eq!(sources, ["backbone:v1", "commander:v1", "can_arm:v1"]);
    assert_eq!(
        report.selection.echo(),
        "robot=real (from file)  recorder=(off)"
    );
    // The base's sim specialization sleeps when the robot is real.
    assert!(
        report
            .skipped
            .iter()
            .any(|s| s.target == "sim_inst" && matches!(s.reason, SkipReason::TargetAbsent))
    );
}

#[test]
fn a_selection_swaps_the_deployed_option_and_pulls_in_fragments_in_order() {
    let dir = tempdir().unwrap();
    let (file, launcher) = composed_fixture(dir.path());
    let (flat, report) = compose(&launcher, &file, &words(&["mujoco"])).expect("composes");

    // Base deployments first, then sim_relays, then the engine.
    let sources: Vec<String> = flat.deployments.iter().map(|d| d.source.label()).collect();
    assert_eq!(
        sources,
        ["backbone:v1", "commander:v1", "arm_sim:v1", "sim_mujoco:v1"]
    );
    // The base adjustment reached the engine instance.
    let sim = flat
        .deployments
        .iter()
        .find(|d| node_name(d) == "sim_mujoco")
        .unwrap();
    assert_eq!(
        sim.instances[0].arguments.get("hardware_version"),
        Some(&AnyType::String("v2".to_owned()))
    );
    // And the relay fragment's adjustment reached the base's backbone.
    let backbone = flat
        .deployments
        .iter()
        .find(|d| node_name(d) == "backbone")
        .unwrap();
    assert_eq!(
        backbone.instances[0].arguments.get("max_ee_velocity_m_s"),
        Some(&AnyType::Float(0.5))
    );
    assert_eq!(report.selection.echo(), "robot=mujoco  recorder=(off)");
}

#[test]
fn fragments_merge_into_a_base_deployment_of_the_same_source() {
    let dir = tempdir().unwrap();
    write(
        &dir.path().join("fragments/cam.json5"),
        &fragment_file(
            r#"
            deployments: [
                { source: { name: "camera", tag: "v1" },
                  instances: [{ instance_id: "wrist" }] },
            ],
        "#,
        ),
    );
    let launcher = write(
        &dir.path().join("l.json5"),
        r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "cams", cardinality: "zero_or_one", provides: ["wrist"],
                  options: { on: "fragments/cam.json5" } },
            ],
            deployments: [
                { source: { name: "camera", tag: "v1" },
                  instances: [{ instance_id: "chest" }] },
            ],
        }"#,
    );
    let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
    let (flat, _) = compose(&parsed, &launcher, &["on".to_string()]).expect("composes");
    assert_eq!(flat.deployments.len(), 1);
    assert_eq!(node_name(&flat.deployments[0]), "camera");
    let ids: Vec<&str> = flat.deployments[0]
        .instances
        .iter()
        .map(|i| i.instance_id.as_str())
        .collect();
    assert_eq!(ids, ["chest", "wrist"]);
}

#[test]
fn a_duplicate_instance_id_names_both_origins() {
    let dir = tempdir().unwrap();
    write(
        &dir.path().join("fragments/dup.json5"),
        &fragment_file(
            r#"
            deployments: [
                { source: { name: "other", tag: "v1" },
                  instances: [{ instance_id: "arm_inst" }] },
            ],
        "#,
        ),
    );
    let launcher = write(
        &dir.path().join("l.json5"),
        r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "robot", cardinality: "zero_or_one", provides: ["arm_inst"],
                  options: { sim: "fragments/dup.json5" } },
            ],
            deployments: [
                { source: { name: "can_arm", tag: "v1" },
                  instances: [{ instance_id: "arm_inst" }] },
            ],
        }"#,
    );
    let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
    let err = compose(&parsed, &launcher, &["sim".to_string()]).expect_err("duplicate id");
    assert!(err.to_string().contains("arm_inst"), "got: {err}");
    assert!(err.to_string().contains("l.json5"), "got: {err}");
    assert!(
        err.to_string().contains("fragments/dup.json5"),
        "got: {err}"
    );
}

#[test]
fn add_links_appends_across_fragments_in_order() {
    let dir = tempdir().unwrap();
    write(
        &dir.path().join("fragments/a.json5"),
        &fragment_file(
            r#"
            deployments: [],
            adjustments: [
                { target: "panel", add_links: { observers: ["a_inst"] } },
            ],
        "#,
        ),
    );
    write(
        &dir.path().join("fragments/b.json5"),
        &fragment_file(
            r#"
            deployments: [],
            adjustments: [
                { target: "panel", add_links: { observers: ["b_inst"] } },
            ],
        "#,
        ),
    );
    let launcher = write(
        &dir.path().join("l.json5"),
        r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "a", cardinality: "zero_or_one", options: { on: "fragments/a.json5" } },
                { name: "b", cardinality: "zero_or_one", options: { on: "fragments/b.json5" } },
            ],
            deployments: [
                { source: { name: "panel", tag: "v1" },
                  instances: [{ instance_id: "panel",
                                links: { observers: ["seed_inst"] } }] },
                { source: { name: "seed", tag: "v1" }, instances: [{ instance_id: "seed_inst" }] },
                { source: { name: "a", tag: "v1" }, instances: [{ instance_id: "a_inst" }] },
                { source: { name: "b", tag: "v1" }, instances: [{ instance_id: "b_inst" }] },
            ],
        }"#,
    );
    let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
    let (flat, report) = compose(
        &parsed,
        &launcher,
        &["a=on".to_string(), "b=on".to_string()],
    )
    .expect("composes");
    let panel = &flat.deployments[0].instances[0];
    let LinkValue::Bound(Selection::Array(targets)) = &panel.links["observers"] else {
        panic!("observers stays an array binding");
    };
    assert_eq!(targets.as_slice(), ["seed_inst", "a_inst", "b_inst"]);
    let added: Vec<&str> = report
        .applied
        .iter()
        .filter(|a| a.change.field() == "links.observers")
        .map(|a| match &a.change {
            AppliedChange::LinkAdded { target, .. } => target.as_str(),
            _ => "",
        })
        .collect();
    assert_eq!(added, ["a_inst", "b_inst"]);
}

#[test]
fn add_links_refuses_a_scalar_and_a_vacancy() {
    for (slot_value, holds) in [
        ("\"x_inst\"", "a scalar binding"),
        ("{ vacant: \"nothing here\" }", "a vacancy"),
    ] {
        let dir = tempdir().unwrap();
        write(
            &dir.path().join("fragments/a.json5"),
            &fragment_file(
                r#"adjustments: [ { target: "panel", add_links: { observers: ["a_inst"] } } ],"#,
            ),
        );
        let launcher = write(
            &dir.path().join("l.json5"),
            &format!(
                r#"{{
                    peppy_schema: "launcher/v1",
                    components: [
                        {{ name: "a", cardinality: "zero_or_one", options: {{ on: "fragments/a.json5" }} }},
                    ],
                    deployments: [
                        {{ source: {{ name: "panel", tag: "v1" }},
                          instances: [{{ instance_id: "panel",
                                        links: {{ observers: {slot_value} }} }}] }},
                        {{ source: {{ name: "a", tag: "v1" }}, instances: [{{ instance_id: "a_inst" }}] }},
                    ],
                }}"#
            ),
        );
        let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
        let err = compose(&parsed, &launcher, &words(&["on"]))
            .expect_err("appending past a non-array must be refused");
        let CompositionError::AddLinksOnNonArray { holds: actual, .. } = &err else {
            panic!("expected AddLinksOnNonArray, got: {err}");
        };
        assert_eq!(actual, &holds);
    }
}

#[test]
fn add_links_refuses_a_target_already_bound() {
    let dir = tempdir().unwrap();
    write(
        &dir.path().join("fragments/a.json5"),
        &fragment_file(
            r#"
            adjustments: [ { target: "panel", add_links: { observers: ["seed_inst"] } } ],
        "#,
        ),
    );
    let launcher = write(
        &dir.path().join("l.json5"),
        r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "a", cardinality: "zero_or_one", options: { on: "fragments/a.json5" } },
            ],
            deployments: [
                { source: { name: "panel", tag: "v1" },
                  instances: [{ instance_id: "panel",
                                links: { observers: ["seed_inst"] } }] },
            ],
        }"#,
    );
    let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
    let err = compose(&parsed, &launcher, &["on".to_string()]).expect_err("duplicate add");
    assert!(err.to_string().contains("seed_inst"), "got: {err}");
}

#[test]
fn replacing_and_appending_to_one_slot_conflict_even_in_one_fragment() {
    let dir = tempdir().unwrap();
    write(
        &dir.path().join("fragments/a.json5"),
        &fragment_file(
            r#"
            adjustments: [
                { target: "panel", set_links: { observers: ["seed_inst"] } },
                { target: "panel", add_links: { observers: ["a_inst"] } },
            ],
        "#,
        ),
    );
    let launcher = write(
        &dir.path().join("l.json5"),
        r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "a", cardinality: "zero_or_one", options: { on: "fragments/a.json5" } },
            ],
            deployments: [
                { source: { name: "panel", tag: "v1" }, instances: [{ instance_id: "panel" }] },
                { source: { name: "seed", tag: "v1" }, instances: [{ instance_id: "seed_inst" }] },
                { source: { name: "a", tag: "v1" }, instances: [{ instance_id: "a_inst" }] },
            ],
        }"#,
    );
    let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
    let err = compose(&parsed, &launcher, &words(&["on"]))
        .expect_err("replace and append are two claims on one entry");
    let CompositionError::AdjustmentsConflict { field, .. } = &err else {
        panic!("expected AdjustmentsConflict, got: {err}");
    };
    assert_eq!(field, "links.observers");
}

#[test]
fn two_fragments_writing_one_key_are_refused() {
    let dir = tempdir().unwrap();
    write(
        &dir.path().join("fragments/a.json5"),
        &fragment_file(
            r#"
            adjustments: [ { target: "panel", set_arguments: { rate: 10 } } ],
        "#,
        ),
    );
    write(
        &dir.path().join("fragments/b.json5"),
        &fragment_file(
            r#"
            adjustments: [ { target: "panel", set_arguments: { rate: 20 } } ],
        "#,
        ),
    );
    let launcher = write(
        &dir.path().join("l.json5"),
        r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "a", cardinality: "zero_or_one", options: { on: "fragments/a.json5" } },
                { name: "b", cardinality: "zero_or_one", options: { on: "fragments/b.json5" } },
            ],
            deployments: [
                { source: { name: "panel", tag: "v1" }, instances: [{ instance_id: "panel" }] },
            ],
        }"#,
    );
    let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
    let err = compose(
        &parsed,
        &launcher,
        &["a=on".to_string(), "b=on".to_string()],
    )
    .expect_err("fragments must not fight");
    let CompositionError::AdjustmentsConflict {
        field,
        first,
        second,
        ..
    } = &err
    else {
        panic!("expected AdjustmentsConflict, got: {err}");
    };
    assert_eq!(field, "arguments.rate");
    assert!(
        first.contains("a.json5") && second.contains("b.json5"),
        "got: {first} / {second}"
    );
}

#[test]
fn the_base_overrides_a_fragment_and_a_later_base_entry_wins() {
    let dir = tempdir().unwrap();
    write(
        &dir.path().join("fragments/a.json5"),
        &fragment_file(
            r#"
            adjustments: [ { target: "panel", set_arguments: { rate: 10 } } ],
        "#,
        ),
    );
    let launcher = write(
        &dir.path().join("l.json5"),
        r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "a", cardinality: "zero_or_one", options: { on: "fragments/a.json5" } },
            ],
            adjustments: [
                { target: "panel", set_arguments: { rate: 20 } },
                { target: "panel", set_arguments: { rate: 30 } },
            ],
            deployments: [
                { source: { name: "panel", tag: "v1" }, instances: [{ instance_id: "panel" }] },
            ],
        }"#,
    );
    let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
    let (flat, report) = compose(&parsed, &launcher, &["on".to_string()]).expect("composes");
    assert_eq!(
        flat.deployments[0].instances[0].arguments.get("rate"),
        Some(&AnyType::Int(30))
    );
    // Both base entries and the fragment's contribution are visible, typed.
    let rates: Vec<(Option<AnyType>, AnyType)> = report
        .applied
        .iter()
        .filter(|a| a.change.field() == "arguments.rate")
        .map(|a| match &a.change {
            AppliedChange::Argument { old, new, .. } => (old.clone(), new.clone()),
            other => panic!("expected a Set, got {other:?}"),
        })
        .collect();
    let int = |value: i64| AnyType::Int(value);
    assert_eq!(
        rates,
        [
            (None, int(10)),
            (Some(int(10)), int(20)),
            (Some(int(20)), int(30)),
        ]
    );
    let lines = report.render_lines();
    assert!(
        lines.iter().any(|line| line.contains("(absent) -> 10")),
        "{lines:?}"
    );
}

#[test]
fn guards_skip_and_absent_targets_skip() {
    let dir = tempdir().unwrap();
    write(
        &dir.path().join("fragments/xr.json5"),
        &fragment_file(
            r#"
            deployments: [
                { source: { name: "xr", tag: "v1" },
                  instances: [{ instance_id: "leader_inst" }] },
            ],
            adjustments: [
                { target: "backbone_inst",
                  set_arguments: { upstream_mode: "pose" } },
            ],
        "#,
        ),
    );
    write(
        &dir.path().join("fragments/rec.json5"),
        &fragment_file(
            r#"
            deployments: [
                { source: { name: "rec", tag: "v1" },
                  instances: [{ instance_id: "recorder_inst" }] },
            ],
            adjustments: [
                // A shape condition: only the XR leader pairs with the
                // backbone this way.
                { target: "backbone_inst", when: { commander: "xr" },
                  set_arguments: { record_backbone: true } },
                // Reaches into whichever robot was selected; skipped when the
                // robot is real and no sim_inst exists.
                { target: "sim_inst",
                  set_arguments: { state_rate_hz: 50 } },
            ],
        "#,
        ),
    );
    let launcher = write(
        &dir.path().join("l.json5"),
        r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "commander",
                  options: { web: { deployments: [] }, xr: "fragments/xr.json5" } },
                { name: "recorder", cardinality: "zero_or_one",
                  options: { on: "fragments/rec.json5" } },
                { name: "robot",
                  options: {
                      real: { deployments: [] },
                      mujoco: { deployments: [
                          { source: { name: "sim_mujoco", tag: "v1" },
                            instances: [{ instance_id: "sim_inst" }] } ] },
                  } },
            ],
            deployments: [
                { source: { name: "backbone", tag: "v1" },
                  instances: [{ instance_id: "backbone_inst" }] },
                { commander: "web" },
                { robot: "real" },
            ],
        }"#,
    );
    let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());

    // web + recorder: the guard is not met and sim_inst is absent, so
    // both of the recorder fragment's adjustments skip.
    let (flat, report) = compose(&parsed, &launcher, &words(&["recorder=on"])).expect("composes");
    assert_eq!(report.skipped.len(), 2);
    assert!(report.skipped.iter().any(|s| matches!(
        s.reason,
        SkipReason::GuardNotMet(ref g) if g == "commander=xr"
    )));
    assert!(
        report
            .skipped
            .iter()
            .any(|s| matches!(s.reason, SkipReason::TargetAbsent))
    );
    let backbone = flat
        .deployments
        .iter()
        .find(|d| node_name(d) == "backbone")
        .unwrap();
    assert!(
        !backbone.instances[0]
            .arguments
            .contains_key("record_backbone")
    );

    // xr + recorder: the guard holds and its target exists, so it
    // applies; sim_inst is still absent and still skips.
    let (flat, report) =
        compose(&parsed, &launcher, &words(&["commander=xr", "recorder=on"])).expect("composes");
    assert!(
        report
            .skipped
            .iter()
            .all(|s| matches!(s.reason, SkipReason::TargetAbsent))
    );
    let backbone = flat
        .deployments
        .iter()
        .find(|d| node_name(d) == "backbone")
        .unwrap();
    assert_eq!(
        backbone.instances[0].arguments.get("record_backbone"),
        Some(&AnyType::Bool(true))
    );

    // xr + recorder + mujoco: every target exists, nothing skips.
    let (_, report) = compose(
        &parsed,
        &launcher,
        &words(&["commander=xr", "recorder=on", "robot=mujoco"]),
    )
    .expect("composes");
    assert!(report.skipped.is_empty());
}

#[test]
fn an_option_missing_a_provides_id_is_refused() {
    let dir = tempdir().unwrap();
    write(
        &dir.path().join("fragments/bad.json5"),
        &fragment_file(
            r#"
            deployments: [
                { source: { name: "other", tag: "v1" }, instances: [{ instance_id: "wrong_inst" }] },
            ],
        "#,
        ),
    );
    let launcher = write(
        &dir.path().join("l.json5"),
        r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "robot", provides: ["arm_inst"],
                  options: { sim: "fragments/bad.json5" } },
            ],
            deployments: [],
        }"#,
    );
    let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
    let err = compose(&parsed, &launcher, &["sim".to_string()]).expect_err("provides unmet");
    let CompositionError::ProvidesUnmet { axis, option, id } = &err else {
        panic!("expected ProvidesUnmet, got: {err}");
    };
    assert_eq!(
        (axis.as_str(), option.as_str(), id.as_str()),
        ("robot", "sim", "arm_inst")
    );
}

#[test]
fn an_adjustment_target_defined_nowhere_is_refused() {
    let dir = tempdir().unwrap();
    write(
        &dir.path().join("fragments/a.json5"),
        &fragment_file(
            r#"
            adjustments: [ { target: "ghost_inst", set_arguments: { x: 1 } } ],
        "#,
        ),
    );
    let launcher = write(
        &dir.path().join("l.json5"),
        r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "a", cardinality: "zero_or_one", options: { on: "fragments/a.json5" } },
            ],
            deployments: [],
        }"#,
    );
    let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
    let err = compose(&parsed, &launcher, &["on".to_string()]).expect_err("dead target");
    let CompositionError::TargetDefinedNowhere { target, .. } = &err else {
        panic!("expected TargetDefinedNowhere, got: {err}");
    };
    assert_eq!(target, "ghost_inst");
}

#[test]
fn unsafe_fragment_paths_are_refused() {
    for raw in ["/etc/passwd", "../outside.json5", "./here.json5", ""] {
        let dir = tempdir().unwrap();
        let spec = FragmentSpec(vec![FragmentPart::File(raw.to_owned())]);
        let axis = ComponentAxis {
            name: "robot".to_owned(),
            options: BTreeMap::from([("sim".to_owned(), spec)]),
            cardinality: Default::default(),
            provides: Vec::new(),
        };
        let launcher = PeppyLauncher {
            peppy_schema: config::schema::PeppySchema::LauncherV1,
            core_nodes: Vec::new(),
            deployments: Vec::new(),
            option_deployments: Vec::new(),
            components: vec![axis],
            adjustments: Vec::new(),
            constraints: Vec::new(),
        };
        let err = compose(&launcher, &dir.path().join("l.json5"), &["sim".to_string()])
            .expect_err("unsafe path must be refused");
        assert!(
            err.to_string().contains("not usable") || err.to_string().contains("cannot be read"),
            "raw path {raw:?}: got: {err}"
        );
        // The origin names the launcher FILE, exactly as the flatten
        // report and the duplicate-id refusal do.
        assert!(
            err.to_string().contains("l.json5"),
            "raw path {raw:?}: got: {err}"
        );
    }
}

#[test]
fn an_inline_only_composition_needs_no_launcher_directory() {
    let launcher = parse_launcher(
        r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "robot",
                  options: { real: { deployments: [
                      { source: { name: "can_arm", tag: "v1" },
                        instances: [{ instance_id: "arm_inst" }] } ] } } },
            ],
            deployments: [{ robot: "real" }],
        }"#,
    );
    // Inline fragments do no I/O, so the launcher's directory is never
    // resolved and need not exist.
    let (flat, _) = compose(
        &launcher,
        Path::new("/nonexistent-peppy-launcher-dir/l.json5"),
        &[],
    )
    .expect("an inline-only composition reads no files");
    assert_eq!(flat.deployments.len(), 1);
    assert_eq!(node_name(&flat.deployments[0]), "can_arm");
}

#[test]
fn a_symlink_escaping_the_launcher_directory_is_refused() {
    let dir = tempdir().unwrap();
    let outside = tempdir().unwrap();
    write(
        &outside.path().join("escape.json5"),
        &fragment_file("deployments: []"),
    );
    fs::create_dir_all(dir.path().join("fragments")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("escape.json5"),
        dir.path().join("fragments/escape.json5"),
    )
    .unwrap();
    let launcher = write(
        &dir.path().join("l.json5"),
        r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "robot", cardinality: "zero_or_one",
                  options: { sim: "fragments/escape.json5" } },
            ],
            deployments: [],
        }"#,
    );
    let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
    let err = compose(&parsed, &launcher, &["sim".to_string()]).expect_err("escape refused");
    assert!(
        err.to_string().contains("leaves the launcher's repository"),
        "got: {err}"
    );
}

#[test]
fn core_nodes_union_base_first() {
    let dir = tempdir().unwrap();
    write(
        &dir.path().join("fragments/a.json5"),
        &fragment_file(
            r#"
            core_nodes: ["edge", "cloud"],
        "#,
        ),
    );
    let launcher = write(
        &dir.path().join("l.json5"),
        r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "a", cardinality: "zero_or_one", options: { on: "fragments/a.json5" } },
            ],
            core_nodes: ["cloud", "base"],
            deployments: [],
        }"#,
    );
    let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
    let (flat, _) = compose(&parsed, &launcher, &["on".to_string()]).expect("composes");
    assert_eq!(flat.core_nodes, ["cloud", "base", "edge"]);
}

// -- selection resolution ------------------------------------------------

fn inline_launcher(components: &str, deployments: &str) -> PeppyLauncher {
    parse_launcher(&format!(
        r#"{{ peppy_schema: "launcher/v1", components: [{components}], deployments: [{deployments}] }}"#
    ))
}

#[test]
fn an_unknown_word_lists_every_axis_with_its_options() {
    let launcher = inline_launcher(
        r#"{ name: "robot", options: { real: { deployments: [] }, mujoco: { deployments: [] } } },
           { name: "commander", options: { web: { deployments: [] } } }"#,
        r#"{ robot: "real" }, { commander: "web" }"#,
    );
    let err =
        compose(&launcher, Path::new("l.json5"), &words(&["isaac"])).expect_err("unknown word");
    let CompositionError::UnknownSelection { menu, .. } = &err else {
        panic!("expected UnknownSelection, got: {err}");
    };
    assert!(menu.contains("robot"), "got: {menu}");
    assert!(menu.contains("`mujoco`, `real`"), "got: {menu}");
    assert!(menu.contains("commander"), "got: {menu}");
}

#[test]
fn an_ambiguous_bare_word_names_the_axes() {
    let launcher = inline_launcher(
        r#"{ name: "robot", options: { none: { deployments: [] } } },
           { name: "recorder", options: { none: { deployments: [] } } }"#,
        r#"{ robot: "none" }, { recorder: "none" }"#,
    );
    let err =
        compose(&launcher, Path::new("l.json5"), &words(&["none"])).expect_err("ambiguous word");
    let CompositionError::AmbiguousSelection { axes: named, .. } = &err else {
        panic!("expected AmbiguousSelection, got: {err}");
    };
    assert!(
        named.contains("`robot`") && named.contains("`recorder`"),
        "got: {named}"
    );
    // The explicit form resolves.
    compose(&launcher, Path::new("l.json5"), &words(&["robot=none"])).expect("explicit form");
}

#[test]
fn two_different_options_on_one_axis_are_refused() {
    let launcher = inline_launcher(
        r#"{ name: "robot", options: { real: { deployments: [] }, mujoco: { deployments: [] } } }"#,
        "",
    );
    let err = compose(
        &launcher,
        Path::new("l.json5"),
        &words(&["mujoco", "robot=real"]),
    )
    .expect_err("conflicting selection");
    let CompositionError::ConflictingSelection {
        axis,
        first,
        second,
    } = &err
    else {
        panic!("expected ConflictingSelection, got: {err}");
    };
    assert_eq!(
        (axis.as_str(), first.as_str(), second.as_str()),
        ("robot", "mujoco", "real")
    );
    // Repeating the same option is one choice.
    compose(
        &launcher,
        Path::new("l.json5"),
        &words(&["mujoco", "robot=mujoco"]),
    )
    .expect("same option twice is one choice");
}

#[test]
fn a_required_axis_the_file_does_not_deploy_needs_a_word() {
    let launcher = inline_launcher(
        r#"{ name: "robot", options: { real: { deployments: [] }, mujoco: { deployments: [] } } }"#,
        "",
    );
    let err = compose(&launcher, Path::new("l.json5"), &[]).expect_err("unresolved axis");
    let CompositionError::UnresolvedAxis { axis, options, .. } = &err else {
        panic!("expected UnresolvedAxis, got: {err}");
    };
    assert_eq!(axis, "robot");
    assert!(options.contains("`real`"), "got: {options}");
    assert!(
        err.to_string().contains(r#"{ robot: "<option>" }"#),
        "got: {err}"
    );
}

#[test]
fn with_on_a_flat_launcher_is_refused() {
    let launcher = parse_launcher(r#"{ peppy_schema: "launcher/v1", deployments: [] }"#);
    let err = compose(
        &launcher,
        Path::new("/tmp/x.json5"),
        &["mujoco".to_string()],
    )
    .expect_err("flat launcher has nothing to select");
    assert!(matches!(err, CompositionError::WithOnFlatLauncher));
}

// -- a fragment's own axes -------------------------------------------------

/// A robot whose fragment declares the commander axis, deploying the web
/// panel unless told otherwise.
fn nested_fixture(dir: &Path, commander_deployment: &str) -> (PathBuf, PeppyLauncher) {
    write(
        &dir.join("fragments/web.json5"),
        &fragment_file(
            r#"deployments: [{ source: { name: "web", tag: "v1" },
                instances: [{ instance_id: "commander_inst", links: { arm: "arm_inst" } }] }]"#,
        ),
    );
    write(
        &dir.join("fragments/xr.json5"),
        &fragment_file(
            r#"deployments: [{ source: { name: "xr", tag: "v1" },
                instances: [{ instance_id: "commander_inst", links: { arm: "arm_inst" } }] }],
               adjustments: [{ target: "arm_inst", when: { simulation: "engine" },
                               set_arguments: { pose_mode: true } }]"#,
        ),
    );
    write(
        &dir.join("fragments/robot.json5"),
        &fragment_file(&format!(
            r#"deployments: [
                 {{ source: {{ name: "arm", tag: "v1" }}, instances: [{{ instance_id: "arm_inst" }}] }},
                 {commander_deployment}
               ],
               components: [
                 {{ name: "commander", provides: ["commander_inst"],
                    options: {{ web: "web.json5", xr: "xr.json5" }} }},
                 {{ name: "recorder", cardinality: "zero_or_one", provides: ["recorder_inst"],
                    options: {{ on: {{ deployments: [{{ source: {{ name: "rec", tag: "v1" }},
                        instances: [{{ instance_id: "recorder_inst", links: {{ arm: "arm_inst" }} }}] }}] }} }} }},
               ],
               constraints: [
                 {{ when: {{ recorder: "on" }}, requires: [{{ commander: "xr" }}],
                    reason: "only the headset can start an episode" }}
               ]"#
        )),
    );
    let launcher = write(
        &dir.join("fleet.json5"),
        r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "simulation", cardinality: "zero_or_one", options: {
                    engine: { deployments: [{ source: { name: "engine", tag: "v1" },
                        instances: [{ instance_id: "engine_inst" }] }] } } },
                { name: "robot", options: { openarm: "fragments/robot.json5" } },
            ],
            deployments: [{ robot: "openarm" }],
        }"#,
    );
    let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
    (launcher, parsed)
}

#[test]
fn a_fragments_axes_are_filled_by_its_deployments_and_reached_by_with() {
    let dir = tempdir().unwrap();
    let (file, launcher) = nested_fixture(dir.path(), r#"{ commander: "web" }"#);
    let (flat, report) = compose(&launcher, &file, &[]).expect("composes");
    let sources: Vec<String> = flat.deployments.iter().map(|d| d.source.label()).collect();
    assert_eq!(sources, ["arm:v1", "web:v1"]);
    assert_eq!(
        report.selection.echo(),
        "simulation=(off)  robot=openarm (from file)  commander=web (from file)  recorder=(off)"
    );

    let (flat, report) = compose(&launcher, &file, &words(&["xr"])).expect("composes");
    let sources: Vec<String> = flat.deployments.iter().map(|d| d.source.label()).collect();
    assert_eq!(sources, ["arm:v1", "xr:v1"]);
    assert_eq!(
        report.selection.echo(),
        "simulation=(off)  robot=openarm (from file)  commander=xr  recorder=(off)"
    );
    // The fragment's guard reads the launcher's axes.
    assert!(
        report
            .skipped
            .iter()
            .any(|s| matches!(&s.reason, SkipReason::GuardNotMet(g) if g == "simulation=engine"))
    );
    let (flat, _) =
        compose(&launcher, &file, &words(&["commander=xr", "engine"])).expect("composes");
    let arm = flat
        .deployments
        .iter()
        .find(|d| node_name(d) == "arm")
        .unwrap();
    assert_eq!(
        arm.instances[0].arguments.get("pose_mode"),
        Some(&AnyType::Bool(true))
    );

    // The fragment's constraint reads the fragment's axes.
    let err =
        compose(&launcher, &file, &words(&["on"])).expect_err("the recorder needs the headset");
    assert!(err.to_string().contains("robot.json5"), "got: {err}");
    assert!(err.to_string().contains("start an episode"), "got: {err}");
    compose(&launcher, &file, &words(&["on", "xr"])).expect("composes");
}

#[test]
fn a_fragment_axis_the_fragment_does_not_deploy_needs_a_word() {
    let dir = tempdir().unwrap();
    let (file, launcher) = nested_fixture(dir.path(), "");
    let err = compose(&launcher, &file, &[]).expect_err("nothing selects the commander");
    let CompositionError::UnresolvedAxis { axis, origin, .. } = &err else {
        panic!("expected UnresolvedAxis, got: {err}");
    };
    assert_eq!(axis, "commander");
    assert_eq!(origin, " of `openarm`");
    compose(&launcher, &file, &words(&["web"])).expect("composes");
}

#[test]
fn a_fragment_two_levels_down_declares_no_axes() {
    let dir = tempdir().unwrap();
    write(
        &dir.path().join("fragments/deep.json5"),
        &fragment_file(r#"components: [{ name: "x", options: { a: {} } }]"#),
    );
    write(
        &dir.path().join("fragments/robot.json5"),
        &fragment_file(
            r#"deployments: [{ source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] }],
               components: [{ name: "commander", options: { deep: "deep.json5" } }]"#,
        ),
    );
    let launcher = write(
        &dir.path().join("fleet.json5"),
        r#"{ peppy_schema: "launcher/v1",
            components: [{ name: "robot", options: { openarm: "fragments/robot.json5" } }],
            deployments: [{ robot: "openarm" }] }"#,
    );
    let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
    let err = PreparedLauncher::load(&parsed, &launcher).expect_err("two levels down is refused");
    assert!(
        matches!(&err, CompositionError::NestedComponents { path, .. } if path == "fragments/deep.json5"),
        "got: {err}"
    );
}

#[test]
fn two_selected_fragments_cannot_declare_the_same_axis() {
    let launcher = inline_launcher(
        r#"{ name: "left", options: { arm: { components: [{ name: "commander", options: { web: {} } }],
             deployments: [{ commander: "web" }] } } },
           { name: "right", options: { arm: { components: [{ name: "commander", options: { web: {} } }],
             deployments: [{ commander: "web" }] } } }"#,
        r#"{ left: "arm" }, { right: "arm" }"#,
    );
    let err =
        compose(&launcher, Path::new("l.json5"), &[]).expect_err("one commander axis in reach");
    assert!(
        matches!(&err, CompositionError::AxisInReachTwice { axis, first, second }
            if axis == "commander" && first == "`left=arm`" && second == "`right=arm`"),
        "got: {err}"
    );
}

// -- constraints ---------------------------------------------------------

/// A robot choice and an optional recorder toggle, all inline: the
/// smallest family the constraints have something to say about.
fn constrained_launcher(constraints: &str) -> PeppyLauncher {
    parse_launcher(&format!(
        r#"{{
            peppy_schema: "launcher/v1",
            components: [
                {{ name: "robot",
                   options: {{
                       real: {{ deployments: [
                           {{ source: {{ name: "can_arm", tag: "v1" }},
                              instances: [{{ instance_id: "arm_inst" }}] }} ] }},
                       mujoco: {{ deployments: [
                           {{ source: {{ name: "sim_arm", tag: "v1" }},
                              instances: [{{ instance_id: "arm_inst" }}] }} ] }},
                   }} }},
                {{ name: "recorder", cardinality: "zero_or_one",
                   options: {{ on: {{ deployments: [
                       {{ source: {{ name: "recorder", tag: "v1" }},
                          instances: [{{ instance_id: "recorder_inst" }}] }} ] }} }} }},
            ],
            deployments: [{{ robot: "real" }}],
            constraints: [{constraints}],
        }}"#
    ))
}

#[test]
fn a_selection_the_constraints_refuse_does_not_launch() {
    let launcher = constrained_launcher(
        r#"{ when: { recorder: "on" }, requires: [{ robot: "real" }],
             reason: "the recorder films the physical rig" }"#,
    );
    let err = compose(
        &launcher,
        Path::new("family.json5"),
        &words(&["mujoco", "on"]),
    )
    .expect_err("the constraint must refuse this member");
    assert!(matches!(
        err,
        CompositionError::ConstraintUnsatisfied { .. }
    ));
    let message = err.to_string();
    assert!(
        message.contains("robot=mujoco  recorder=on"),
        "got: {message}"
    );
    assert!(
        message.contains("the launcher selecting recorder=on"),
        "got: {message}"
    );
    assert!(message.contains("`robot=real`"), "got: {message}");
    assert!(
        message.contains("films the physical rig"),
        "the author's reason must reach the operator, got: {message}"
    );
}

/// The file's entry fills an axis exactly as an explicit word does, and the
/// refusal's echo marks it, so the operator sees which axis they never
/// named.
#[test]
fn a_deployed_axis_can_violate_a_constraint_and_the_echo_marks_it() {
    let launcher = constrained_launcher(
        r#"{ when: { recorder: "on" }, requires: [{ robot: "mujoco" }],
             reason: "this recorder observes only the simulated arm" }"#,
    );
    let err = compose(&launcher, Path::new("family.json5"), &words(&["on"]))
        .expect_err("the deployed robot violates the constraint");
    assert!(
        err.to_string().contains("robot=real (from file)"),
        "got: {err}"
    );
}

#[test]
fn a_selection_satisfying_the_constraints_flattens() {
    let launcher = constrained_launcher(
        r#"{ when: { recorder: "on" }, requires: [{ robot: "real" }],
             reason: "the recorder films the physical rig" }"#,
    );
    // The file's robot satisfies the requirement...
    compose(&launcher, Path::new("family.json5"), &words(&["on"]))
        .expect("real (from file) + recorder satisfies the constraint");
    // ...and a selection the guard does not speak about is untouched.
    compose(&launcher, Path::new("family.json5"), &words(&["mujoco"]))
        .expect("with the recorder off the constraint is quiet");
    // An alternative list is an OR: the second alternative carries it.
    let either = constrained_launcher(
        r#"{ when: { robot: "mujoco" }, requires: [{ recorder: "on" }],
             reason: "the sim exists to produce datasets" }"#,
    );
    compose(
        &either,
        Path::new("family.json5"),
        &words(&["mujoco", "on"]),
    )
    .expect("the required recorder is selected");
}

#[test]
fn a_forbidden_combination_is_refused_and_names_the_matched_entry() {
    let launcher = constrained_launcher(
        r#"{ when: { robot: "mujoco" }, forbids: [{ recorder: "on" }],
             reason: "the recorder films only the physical rig" }"#,
    );
    let err = compose(
        &launcher,
        Path::new("family.json5"),
        &words(&["mujoco", "on"]),
    )
    .expect_err("the exclusion must refuse this member");
    assert!(matches!(err, CompositionError::ConstraintForbidden { .. }));
    let message = err.to_string();
    assert!(message.contains("selecting robot=mujoco"), "got: {message}");
    assert!(message.contains("forbids `recorder=on`"), "got: {message}");
    assert!(
        message.contains("films only the physical rig"),
        "got: {message}"
    );
    // Either side alone is untouched.
    compose(&launcher, Path::new("family.json5"), &words(&["mujoco"]))
        .expect("the sim without the recorder is fine");
    compose(&launcher, Path::new("family.json5"), &words(&["on"]))
        .expect("the recorder on the real (from file) robot is fine");
}

/// The unconditional form: one entry naming the whole combination, with
/// no `when` at all.
#[test]
fn an_unconditional_forbid_refuses_exactly_its_combination() {
    let launcher = constrained_launcher(
        r#"{ forbids: [{ robot: "mujoco", recorder: "on" }],
             reason: "the recorder films only the physical rig" }"#,
    );
    let err = compose(
        &launcher,
        Path::new("family.json5"),
        &words(&["mujoco", "on"]),
    )
    .expect_err("the combination is forbidden");
    assert!(
        err.to_string()
            .contains("the launcher forbids `recorder=on robot=mujoco`"),
        "got: {err}"
    );
    compose(&launcher, Path::new("family.json5"), &words(&["mujoco"]))
        .expect("half the combination is not the combination");
    let problems = check_composition(&launcher, Path::new("family.json5"));
    assert!(problems.is_empty(), "got: {problems:?}");
}

/// The gap `repo index --check` closes: a selection the constraints
/// refuse is not required to flatten. This family's refused member is
/// exactly its broken one (the recorder's append lands on a slot the
/// mujoco arm binds as a scalar), so without the constraint the check
/// fails and with it the check passes.
#[test]
fn check_composition_holds_only_legal_selections_to_flattening() {
    let family = |constraints: &str| {
        parse_launcher(&format!(
            r#"{{
                peppy_schema: "launcher/v1",
                components: [
                    {{ name: "robot",
                       options: {{
                           real: {{ deployments: [
                               {{ source: {{ name: "can_arm", tag: "v1" }},
                                  instances: [{{ instance_id: "arm_inst" }}] }} ] }},
                           mujoco: {{ deployments: [
                               {{ source: {{ name: "sim_engine", tag: "v1" }},
                                  instances: [{{ instance_id: "sim_inst" }}] }},
                               {{ source: {{ name: "sim_arm", tag: "v1" }},
                                  instances: [{{ instance_id: "arm_inst",
                                                links: {{ observed: "sim_inst" }} }}] }} ] }},
                       }} }},
                    {{ name: "recorder", cardinality: "zero_or_one",
                       options: {{ on: {{
                           deployments: [
                               {{ source: {{ name: "recorder", tag: "v1" }},
                                  instances: [{{ instance_id: "recorder_inst" }}] }} ],
                           adjustments: [
                               {{ target: "arm_inst",
                                  add_links: {{ observed: ["recorder_inst"] }} }} ],
                       }} }} }},
                ],
                deployments: [{{ robot: "real" }}],
                {constraints}
            }}"#
        ))
    };

    let unconstrained = family("");
    let problems = check_composition(&unconstrained, Path::new("family.json5"));
    assert_eq!(problems.len(), 1, "got: {problems:?}");
    assert!(
        problems[0].contains("robot=mujoco  recorder=on"),
        "got: {problems:?}"
    );

    let constrained = family(
        r#"constraints: [
            { when: { recorder: "on" }, requires: [{ robot: "real" }],
              reason: "the recorder films the physical rig" } ],"#,
    );
    let problems = check_composition(&constrained, Path::new("family.json5"));
    assert!(problems.is_empty(), "got: {problems:?}");
}

#[test]
fn check_composition_flags_an_option_nothing_may_select() {
    let launcher = constrained_launcher(
        r#"{ when: { robot: "mujoco" }, requires: [{ recorder: "on" }],
             reason: "the sim exists to produce datasets" },
           { when: { recorder: "on" }, requires: [{ robot: "real" }],
             reason: "the recorder films the physical rig" }"#,
    );
    let problems = check_composition(&launcher, Path::new("family.json5"));
    assert_eq!(problems.len(), 1, "got: {problems:?}");
    assert!(
        problems[0].contains("fill axis `robot` with `mujoco`"),
        "got: {problems:?}"
    );
}

#[test]
fn check_composition_flags_a_constraint_that_refuses_nothing() {
    let launcher = constrained_launcher(
        r#"{ when: { recorder: "on" }, requires: [{ robot: "real" }, { robot: "mujoco" }],
             reason: "some robot must be selected" }"#,
    );
    let problems = check_composition(&launcher, Path::new("family.json5"));
    assert_eq!(problems.len(), 1, "got: {problems:?}");
    assert!(
        problems[0].contains("refuses no selection"),
        "got: {problems:?}"
    );
}

/// A dead constraint shadowed by an earlier one is reported with the
/// constraint in front named: the rule that makes the later one dead.
#[test]
fn check_composition_names_the_constraint_that_shadows_a_dead_one() {
    let launcher = constrained_launcher(
        r#"{ forbids: [{ robot: "mujoco" }], reason: "the sim is retired" },
           { when: { recorder: "on" }, forbids: [{ robot: "mujoco" }],
             reason: "the recorder films only the physical rig" }"#,
    );
    let problems = check_composition(&launcher, Path::new("family.json5"));
    // The broad first rule strangles `mujoco` outright (a dead option)
    // and stands in front of the narrow second one (a dead constraint).
    assert_eq!(problems.len(), 2, "got: {problems:?}");
    assert!(
        problems
            .iter()
            .any(|p| p.contains("fill axis `robot` with `mujoco`")),
        "got: {problems:?}"
    );
    let dead = problems
        .iter()
        .find(|p| p.contains("refuses no selection"))
        .expect("the shadowed constraint is dead");
    assert!(
        dead.contains("the launcher forbids `robot=mujoco` refuses them first"),
        "got: {dead}"
    );
}

#[test]
fn check_composition_flags_a_refused_bare_launch() {
    let launcher = constrained_launcher(
        r#"{ when: { robot: "real" }, requires: [{ recorder: "on" }],
             reason: "the rig is always recorded" }"#,
    );
    let problems = check_composition(&launcher, Path::new("family.json5"));
    assert_eq!(problems.len(), 1, "got: {problems:?}");
    assert!(problems[0].contains("bare launch"), "got: {problems:?}");
    assert!(
        problems[0].contains("robot=real (from file)"),
        "got: {problems:?}"
    );
}

/// The bare launch is one member checked on its own, so it is still
/// checked when the family is too big to enumerate: the ceiling guard
/// skips the cross-combination checks, not the bare launch.
#[test]
fn check_composition_flags_a_refused_bare_launch_above_the_ceiling() {
    // Enough binary axes to exceed the enumeration budget.
    let count = 2048usize.ilog2() + 1;
    let axes = (0..count)
        .map(|i| {
            format!(
                r#"{{ name: "a{i}", options: {{ one: {{ deployments: [] }}, two: {{ deployments: [] }} }} }}"#
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let deployed = (0..count)
        .map(|i| format!(r#"{{ a{i}: "one" }}"#))
        .collect::<Vec<_>>()
        .join(", ");
    let launcher = parse_launcher(&format!(
        r#"{{
            peppy_schema: "launcher/v1",
            components: [{axes}],
            deployments: [{deployed}],
            constraints: [
                {{ when: {{ a0: "one" }}, requires: [{{ a1: "two" }}],
                   reason: "the all-defaults member is not one anyone may run" }} ],
        }}"#
    ));
    let problems = check_composition(&launcher, Path::new("family.json5"));
    assert_eq!(problems.len(), 2, "got: {problems:?}");
    assert!(
        problems.iter().any(|p| p.contains("bare launch")),
        "got: {problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains("cross-combination checks")),
        "got: {problems:?}"
    );
}

#[test]
fn check_composition_flags_an_optional_axis_that_may_never_stay_off() {
    let launcher = constrained_launcher(
        r#"{ requires: [{ recorder: "on" }], reason: "every session is recorded" }"#,
    );
    let problems = check_composition(&launcher, Path::new("family.json5"));
    // Two findings, one cause: `zero_or_one` is a promise nothing keeps,
    // and the bare launch it implies is refused.
    assert_eq!(problems.len(), 2, "got: {problems:?}");
    assert!(
        problems
            .iter()
            .any(|p| p.contains("leave axis `recorder` unfilled")),
        "got: {problems:?}"
    );
    assert!(
        problems.iter().any(|p| p.contains("bare launch")),
        "got: {problems:?}"
    );
}

#[test]
fn check_composition_checks_a_fragments_own_axes_and_copies() {
    let dir = tempdir().unwrap();
    let (file, launcher) = nested_fixture(dir.path(), r#"{ commander: "web" }"#);
    let problems = check_composition(&launcher, &file);
    assert!(problems.is_empty(), "got: {problems:?}");

    // A copy whose fragment constraint can never be satisfied leaves a dead
    // option behind.
    let fleet = write(
        &dir.path().join("copies.json5"),
        r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "simulation", cardinality: "zero_or_one", options: {
                    engine: { deployments: [{ source: { name: "engine", tag: "v1" },
                        instances: [{ instance_id: "engine_inst" }] }] } } },
                { name: "robot", cardinality: "zero_or_more", options: {
                    openarm: "fragments/robot.json5",
                    broken: { deployments: [{ source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] }],
                              constraints: [{ requires: [{ robot: "openarm" }], reason: "never" }] },
                } },
            ],
        }"#,
    );
    let parsed = parse_launcher(&fs::read_to_string(&fleet).unwrap());
    let problems = check_composition(&parsed, &fleet);
    assert!(
        problems
            .iter()
            .any(|p| p.contains("fill axis `robot` with `broken`")),
        "got: {problems:?}"
    );
}

/// The check's own copy name stays clear of a launcher's core node links
/// and instance ids.
#[test]
fn check_composition_names_its_copy_clear_of_the_launcher() {
    let dir = tempdir().unwrap();
    let fleet = write(
        &dir.path().join("placed.json5"),
        r#"{
            peppy_schema: "launcher/v1",
            core_nodes: ["composition_check"],
            components: [
                { name: "robot", cardinality: "zero_or_more", options: {
                    arm: { deployments: [{ source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] }] } } },
            ],
            deployments: [{ source: { name: "engine", tag: "v1" },
                instances: [{ instance_id: "composition_check_engine_inst", core_node: "composition_check" }] }],
        }"#,
    );
    let parsed = parse_launcher(&fs::read_to_string(&fleet).unwrap());
    let problems = check_composition(&parsed, &fleet);
    assert!(problems.is_empty(), "got: {problems:?}");

    let prefixed = write(
        &dir.path().join("prefixed.json5"),
        r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "robot", cardinality: "zero_or_more", options: {
                    arm: { deployments: [{ source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] }] } } },
            ],
            deployments: [{ source: { name: "engine", tag: "v1" },
                instances: [{ instance_id: "composition_check_arm_inst" }] }],
        }"#,
    );
    let parsed = parse_launcher(&fs::read_to_string(&prefixed).unwrap());
    let problems = check_composition(&parsed, &prefixed);
    assert!(problems.is_empty(), "got: {problems:?}");
}

#[test]
fn constraints_without_components_are_refused() {
    let err = PeppyLauncherParser::from_content(
        r#"{
            peppy_schema: "launcher/v1",
            deployments: [],
            constraints: [
                { requires: [{ robot: "real" }], reason: "r" } ],
        }"#,
    )
    .expect_err("a flat launcher has no selection to refuse");
    assert!(
        err.to_string()
            .contains("declares `constraints` but no `components`"),
        "got: {err}"
    );
}

#[test]
fn a_composed_launcher_round_trips_its_deployments() {
    let dir = tempdir().unwrap();
    let (_, launcher) = nested_fixture(dir.path(), r#"{ commander: "web" }"#);
    let written = serde_json5::to_string(&launcher).unwrap();
    let reparsed: PeppyLauncher = serde_json5::from_str(&written).unwrap();
    assert_eq!(reparsed.option_deployments, launcher.option_deployments);
    assert_eq!(reparsed.components.len(), launcher.components.len());
}

/// Two fragment files sharing a name in different directories are two
/// documents: the check judges each by its own constraints.
#[test]
fn the_check_tells_apart_fragment_files_that_share_a_name() {
    let dir = tempdir().unwrap();
    for (side, rule) in [
        ("a", r#"requires: [{ simulation: "engine" }]"#),
        ("b", r#"forbids: [{ simulation: "engine" }]"#),
    ] {
        write(
            &dir.path().join(format!("fragments/{side}/web.json5")),
            &fragment_file(&format!(
                r#"deployments: [{{ source: {{ name: "web_{side}", tag: "v1" }}, instances: [{{ instance_id: "commander_inst" }}] }}],
                   constraints: [{{ when: {{ commander: "web" }}, {rule}, reason: "{side} says so" }}]"#
            )),
        );
        write(
            &dir.path().join(format!("fragments/{side}/robot.json5")),
            &fragment_file(
                r#"deployments: [{ source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] },
                                { commander: "web" }],
                   components: [{ name: "commander", options: { web: "web.json5" } }]"#,
            ),
        );
    }
    let launcher = write(
        &dir.path().join("fleet.json5"),
        r#"{ peppy_schema: "launcher/v1",
            components: [
                { name: "simulation", cardinality: "zero_or_one", options: { engine: { deployments: [
                    { source: { name: "engine", tag: "v1" }, instances: [{ instance_id: "engine_inst" }] } ] } } },
                { name: "robot", options: { a: "fragments/a/robot.json5", b: "fragments/b/robot.json5" } }
            ],
            deployments: [{ robot: "b" }] }"#,
    );
    let parsed = parse_launcher(&fs::read_to_string(&launcher).unwrap());
    compose(&parsed, &launcher, &words(&["b"])).expect("b without the engine");
    compose(&parsed, &launcher, &words(&["b", "engine"])).expect_err("b forbids the engine");
    let problems = check_composition(&parsed, &launcher);
    assert!(problems.is_empty(), "{problems:?}");
}

/// The check holds every enumerated selection to the reach rule a launch
/// applies.
#[test]
fn the_check_reports_axes_two_selected_options_both_declare() {
    let launcher = inline_launcher(
        r#"{ name: "left", cardinality: "zero_or_one", options: { arm: { components: [{ name: "commander", options: { web: {} } }],
             deployments: [{ commander: "web" }] } } },
           { name: "right", cardinality: "zero_or_one", options: { arm: { components: [{ name: "commander", options: { web: {} } }],
             deployments: [{ commander: "web" }] } } }"#,
        "",
    );
    let problems = check_composition(&launcher, Path::new("l.json5"));
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("axis `commander` is declared by both")),
        "{problems:?}"
    );
}
