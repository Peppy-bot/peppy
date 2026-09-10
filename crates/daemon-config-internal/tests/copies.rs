use config::{AnyType, runtime::Name};
use core_node_api::encoding::ArgumentOverride;
use daemon_config::launcher::{
    ComposedLaunch, CompositionError, JoinRequest, LinkValue, PeppyLauncher, PeppyLauncherParser,
    PreparedLauncher, RunningStack, Selection, SkipReason, SkippedAdjustment, UnitSelection,
};
use std::path::Path;

mod common;
use common::{fragment_file, words, write};

/// A fleet: one stack axis holding the engine, one axis running as copies
/// holding the robot, whose fragments carry the commander axis of their own.
fn fleet(deployments: &str) -> PreparedLauncher {
    load(&fleet_document(deployments))
}

fn fleet_document(deployments: &str) -> String {
    format!(
        r#"{{
        peppy_schema: "launcher/v1",
        components: [
            {{ name: "simulation", cardinality: "zero_or_one", options: {{
                engine: {{ deployments: [
                    {{ source: {{ name: "engine", tag: "v1" }}, instances: [
                        {{ instance_id: "engine_inst" }}
                    ] }}
                ] }}
            }} }},
            {{ name: "robot", cardinality: "zero_or_more", options: {{
                real: {{
                    deployments: [
                        {{ source: {{ name: "arm", tag: "v1" }}, instances: [
                            {{ instance_id: "arm_inst", arguments: {{ speed: 0.25 }} }}
                        ] }},
                        {{ commander: "web" }},
                    ],
                    components: [{COMMANDER}],
                }},
                sim: {{
                    deployments: [
                        {{ source: {{ name: "arm", tag: "v1" }}, instances: [
                            {{ instance_id: "arm_inst", arguments: {{ speed: 0.25 }},
                              links: {{ engine: "engine_inst" }} }}
                        ] }},
                        {{ commander: "web" }},
                    ],
                    components: [{COMMANDER}],
                }},
                empty: {{}},
            }} }}
        ],
        constraints: [
            {{ when: {{ robot: "sim" }}, requires: [{{ simulation: "engine" }}],
              reason: "a simulated arm needs the engine" }}
        ],
        adjustments: [
            {{ target: "arm_inst", when: {{ robot: "sim" }}, set_arguments: {{ speed: 0.5 }} }},
            {{ target: "commander_inst", when: {{ robot: "sim" }},
              set_links: {{ arm: "arm_inst/control" }}, add_links: {{ arms: ["engine_inst"] }} }}
        ],
        deployments: [{deployments}]
    }}"#,
        COMMANDER = r#"{ name: "commander", provides: ["commander_inst"], options: {
            web: { deployments: [
                { source: { name: "commander", tag: "v1" }, instances: [
                    { instance_id: "commander_inst",
                      links: { arm: "arm_inst/control", arms: ["arm_inst"] } }
                ] }
            ] },
            xr: { deployments: [
                { source: { name: "headset", tag: "v1" }, instances: [
                    { instance_id: "commander_inst", links: { arm: "arm_inst/control", arms: ["arm_inst"] } }
                ] }
            ] }
        } }"#
    )
}

fn load(document: &str) -> PreparedLauncher {
    let parsed = PeppyLauncherParser::from_content(document).unwrap();
    PreparedLauncher::load(&parsed, Path::new("fleet.json5")).unwrap()
}

fn name(value: &str) -> Name {
    Name::try_from(value.to_owned()).unwrap()
}

fn ids(flat: &PeppyLauncher) -> Vec<String> {
    flat.deployments
        .iter()
        .flat_map(|d| &d.instances)
        .map(|i| i.instance_id.to_string())
        .collect()
}

fn instance<'a>(
    flat: &'a PeppyLauncher,
    id: &str,
) -> &'a daemon_config::launcher::DeploymentInstance {
    flat.deployments
        .iter()
        .flat_map(|d| &d.instances)
        .find(|i| i.instance_id.as_str() == id)
        .unwrap_or_else(|| panic!("{id} is in the plan"))
}

/// A join with no words and no overrides.
fn join(
    prepared: &PreparedLauncher,
    option: &str,
    copy: &str,
    stack: &UnitSelection,
    existing: &PeppyLauncher,
) -> Result<PeppyLauncher, CompositionError> {
    prepared
        .join(
            JoinRequest {
                option,
                name: &name(copy),
                words: &[],
                arguments: &[],
            },
            RunningStack {
                selection: stack,
                launcher: existing,
            },
        )
        .map(|joined| joined.launcher)
}

#[test]
fn one_two_and_six_robots_share_the_engine_and_keep_their_own_wiring() {
    let prepared = fleet("");
    let ComposedLaunch {
        launcher: mut flat,
        selection,
        ..
    } = prepared.launch(&words(&["engine"])).unwrap();
    assert_eq!(ids(&flat), ["engine_inst"]);
    for (index, robot) in ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"]
        .iter()
        .enumerate()
    {
        flat = join(&prepared, "sim", robot, &selection, &flat).unwrap();
        assert_eq!(ids(&flat).len(), 1 + 2 * (index + 1));
        let arm = instance(&flat, &format!("{robot}_arm_inst"));
        assert_eq!(arm.arguments["speed"], AnyType::Float(0.5));
        assert_eq!(
            arm.links["engine"].selection().unwrap().targets(),
            &["engine_inst"]
        );
        assert_eq!(arm.core_node.as_deref(), Some(*robot));
        let commander = instance(&flat, &format!("{robot}_commander_inst"));
        assert_eq!(
            commander.links["arm"].selection().unwrap().targets(),
            &[format!("{robot}_arm_inst/control")]
        );
        assert_eq!(
            commander.links["arms"].selection().unwrap().targets(),
            &[format!("{robot}_arm_inst"), "engine_inst".to_owned()]
        );
        assert!(flat.core_nodes.iter().any(|link| link == robot));
    }
}

#[test]
fn a_copy_selects_its_options_own_axes_at_join() {
    let prepared = fleet("");
    let launch = prepared.launch(&words(&["engine"])).unwrap();
    let joined = prepared
        .join(
            JoinRequest {
                option: "sim",
                name: &name("alpha"),
                words: &words(&["xr"]),
                arguments: &[],
            },
            RunningStack {
                selection: &launch.selection,
                launcher: &launch.launcher,
            },
        )
        .unwrap();
    assert_eq!(joined.copy.selection.echo(), "robot=sim  commander=xr");
    assert_eq!(
        joined.copy.instance_ids,
        [name("alpha_arm_inst"), name("alpha_commander_inst")]
    );
    let commander = joined
        .launcher
        .deployments
        .iter()
        .find(|d| d.source.label() == "headset:v1")
        .expect("the headset commander was selected");
    assert_eq!(
        commander.instances[0].instance_id.as_str(),
        "alpha_commander_inst"
    );
    let explicit = prepared
        .join(
            JoinRequest {
                option: "sim",
                name: &name("bravo"),
                words: &words(&["commander=xr"]),
                arguments: &[],
            },
            RunningStack {
                selection: &launch.selection,
                launcher: &joined.launcher,
            },
        )
        .unwrap();
    assert_eq!(explicit.copy.selection.echo(), "robot=sim  commander=xr");
    let error = prepared
        .join(
            JoinRequest {
                option: "sim",
                name: &name("charlie"),
                words: &words(&["engine"]),
                arguments: &[],
            },
            RunningStack {
                selection: &launch.selection,
                launcher: &joined.launcher,
            },
        )
        .unwrap_err();
    assert!(
        matches!(&error, CompositionError::UnknownCopySelection { word, option, menu }
            if word == "engine" && option == "sim" && menu.contains("commander: `web`, `xr`")),
        "{error}"
    );
}

#[test]
fn join_overrides_apply_after_guarded_launcher_adjustments() {
    let prepared = fleet("");
    let launch = prepared.launch(&words(&["engine"])).unwrap();
    let overrides: Vec<ArgumentOverride> = vec!["arm_inst.speed=0.2".parse().unwrap()];
    let joined = prepared
        .join(
            JoinRequest {
                option: "sim",
                name: &name("alpha"),
                words: &[],
                arguments: &overrides,
            },
            RunningStack {
                selection: &launch.selection,
                launcher: &launch.launcher,
            },
        )
        .unwrap();
    assert_eq!(
        instance(&joined.launcher, "alpha_arm_inst").arguments["speed"],
        AnyType::Float(0.2)
    );
    let lines = joined.report.render_lines().join("\n");
    assert!(
        lines.contains(
            "alpha_commander_inst.links.arm: \"alpha_arm_inst/control\" -> \"alpha_arm_inst/control\""
        ),
        "{lines}"
    );
    assert!(
        lines.contains("alpha_commander_inst.links.arms: + engine_inst"),
        "{lines}"
    );
    assert!(joined.report.applied.iter().any(|entry| {
        entry.target == "alpha_arm_inst"
            && entry.change.field() == "arguments.speed"
            && entry.origin == "arguments of copy `alpha`"
    }));
    let twice: Vec<ArgumentOverride> = vec![
        "arm_inst.speed=0.2".parse().unwrap(),
        "arm_inst.speed=0.3".parse().unwrap(),
    ];
    let error = prepared
        .join(
            JoinRequest {
                option: "sim",
                name: &name("bravo"),
                words: &[],
                arguments: &twice,
            },
            RunningStack {
                selection: &launch.selection,
                launcher: &joined.launcher,
            },
        )
        .unwrap_err();
    assert!(
        matches!(&error, CompositionError::DuplicateArgumentOverride { target, argument }
            if target == "arm_inst" && argument == "speed"),
        "{error}"
    );
    let foreign: Vec<ArgumentOverride> = vec!["engine_inst.speed=1".parse().unwrap()];
    let error = prepared
        .join(
            JoinRequest {
                option: "sim",
                name: &name("bravo"),
                words: &[],
                arguments: &foreign,
            },
            RunningStack {
                selection: &launch.selection,
                launcher: &joined.launcher,
            },
        )
        .unwrap_err();
    assert!(
        matches!(&error, CompositionError::ArgumentTargetAbsent { copy, target, .. }
            if copy == "bravo" && target == "engine_inst"),
        "{error}"
    );
}

#[test]
fn copies_are_never_selected_by_a_launch_word() {
    let prepared = fleet("");
    let error = prepared.launch(&words(&["real"])).unwrap_err();
    assert!(
        matches!(&error, CompositionError::RepeatableAxisAtLaunch { word, axis, option }
            if word == "real" && axis == "robot" && option == "real"),
        "{error}"
    );
    assert!(
        error.to_string().contains("peppy stack join real -i NAME"),
        "{error}"
    );
    let error = prepared.launch(&words(&["xr"])).unwrap_err();
    assert!(
        matches!(&error, CompositionError::CopyAxisAtLaunch { word, axis, parent, option }
            if word == "xr" && axis == "commander" && option == "xr" && (parent == "real" || parent == "sim")),
        "{error}"
    );
}

#[test]
fn the_constraints_see_the_stack_beside_the_copy() {
    let prepared = fleet("");
    let launch = prepared.launch(&[]).unwrap();
    assert!(ids(&launch.launcher).is_empty());
    assert_eq!(launch.selection.echo(), "simulation=(off)");
    let error = join(
        &prepared,
        "sim",
        "alpha",
        &launch.selection,
        &launch.launcher,
    )
    .unwrap_err();
    assert!(error.to_string().contains("needs the engine"), "{error}");
    let joined = join(
        &prepared,
        "real",
        "alpha",
        &launch.selection,
        &launch.launcher,
    )
    .unwrap();
    assert_eq!(ids(&joined), ["alpha_arm_inst", "alpha_commander_inst"]);
    let error = join(&prepared, "real", "alpha", &launch.selection, &joined).unwrap_err();
    assert!(
        matches!(&error, CompositionError::NameIsCoreNodeLink { name } if name == "alpha"),
        "{error}"
    );
}

#[test]
fn the_copies_a_file_deploys_match_a_launch_followed_by_joins() {
    let prepared = fleet(
        r#"{ robot: "sim", instances: [
            { instance_id: "alpha", arguments: { arm_inst: { speed: 0.2 } } },
            { instance_id: "bravo", with: { commander: "xr" } },
        ] }"#,
    );
    let launch = prepared.launch(&words(&["engine"])).unwrap();
    let copies = launch.copies();
    assert_eq!(
        copies
            .iter()
            .map(|copy| copy.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "bravo"]
    );
    assert_eq!(
        copies[0].instance_ids,
        [name("alpha_arm_inst"), name("alpha_commander_inst")]
    );
    assert_eq!(
        copies[0].selection.echo(),
        "robot=sim  commander=web (from file)"
    );
    assert_eq!(copies[1].selection.echo(), "robot=sim  commander=xr");
    assert_eq!(launch.selection.echo(), "simulation=engine");

    let bare = fleet("");
    let stack = bare.launch(&words(&["engine"])).unwrap();
    let overrides: Vec<ArgumentOverride> = vec!["arm_inst.speed=0.2".parse().unwrap()];
    let one = bare
        .join(
            JoinRequest {
                option: "sim",
                name: &name("alpha"),
                words: &[],
                arguments: &overrides,
            },
            RunningStack {
                selection: &stack.selection,
                launcher: &stack.launcher,
            },
        )
        .unwrap();
    let two = bare
        .join(
            JoinRequest {
                option: "sim",
                name: &name("bravo"),
                words: &words(&["xr"]),
                arguments: &[],
            },
            RunningStack {
                selection: &stack.selection,
                launcher: &one.launcher,
            },
        )
        .unwrap();
    assert_eq!(
        serde_json::to_value(&launch.launcher).unwrap(),
        serde_json::to_value(&two.launcher).unwrap()
    );
    let lines = launch.report.render_lines();
    assert!(
        lines.contains(&String::from(
            "copy alpha: robot=sim  commander=web (from file)"
        )),
        "{lines:?}"
    );

    // The same copies join a stack with no engine only as real arms.
    let error = prepared.launch(&[]).unwrap_err();
    assert!(error.to_string().contains("needs the engine"), "{error}");
}

#[test]
fn a_copy_starts_something_and_selects_what_its_option_declares() {
    let prepared = fleet("");
    let launch = prepared.launch(&[]).unwrap();
    let error = join(
        &prepared,
        "empty",
        "alpha",
        &launch.selection,
        &launch.launcher,
    )
    .unwrap_err();
    assert!(
        matches!(&error, CompositionError::CopyStartsNothing { copy, option }
            if copy == "alpha" && option == "empty"),
        "{error}"
    );
    let error = PeppyLauncherParser::from_content(
        r#"{ peppy_schema: "launcher/v1", components: [
            { name: "robot", cardinality: "zero_or_more", options: { real: {} } }
        ], deployments: [{ robot: "real", instances: [{ instance_id: "alpha", with: { commander: "web" } }] }] }"#,
    );
    let prepared = PreparedLauncher::load(&error.unwrap(), Path::new("fleet.json5"));
    assert!(
        matches!(prepared, Err(CompositionError::CopySelectsUnknownAxis { ref copy, ref axis, .. })
            if copy == "alpha" && axis == "commander"),
        "{prepared:?}"
    );
    let stack_only = load(
        r#"{ peppy_schema: "launcher/v1", components: [
            { name: "robot", options: { real: { deployments: [
                { source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] }
            ] } } }
        ], deployments: [{ robot: "real" }] }"#,
    );
    let launch = stack_only.launch(&[]).unwrap();
    assert!(matches!(
        join(
            &stack_only,
            "real",
            "alpha",
            &launch.selection,
            &launch.launcher
        ),
        Err(CompositionError::NoRepeatableAxis)
    ));
    let fleet = fleet("");
    let launch = fleet.launch(&[]).unwrap();
    let error = join(
        &fleet,
        "engine",
        "alpha",
        &launch.selection,
        &launch.launcher,
    )
    .unwrap_err();
    assert!(
        matches!(&error, CompositionError::JoinUnknownOption { option, menu }
            if option == "engine" && menu.contains("robot: `empty`, `real`, `sim`")),
        "{error}"
    );
}

#[test]
fn flat_launchers_keep_their_selection_errors() {
    let prepared = load(r#"{peppy_schema: "launcher/v1", deployments: []}"#);
    assert!(prepared.launch(&[]).is_ok());
    assert!(matches!(
        prepared.launch(&words(&["unknown"])),
        Err(CompositionError::WithOnFlatLauncher)
    ));
}

#[test]
fn the_axis_grammar_names_the_replacement_for_every_refused_key() {
    let document =
        |axis: &str| format!(r#"{{ peppy_schema: "launcher/v1", components: [{axis}] }}"#);
    let error = PeppyLauncherParser::from_content(&document(
        r#"{ name: "robot", optional: true, options: { on: {} } }"#,
    ))
    .unwrap_err()
    .to_string();
    assert!(error.contains("cardinality: \"zero_or_one\""), "{error}");
    let error = PeppyLauncherParser::from_content(&document(
        r#"{ name: "robot_system", cardinality: "zero_or_more", components: [
            { name: "robot", options: { on: {} } }
        ] }"#,
    ))
    .unwrap_err()
    .to_string();
    assert!(error.contains("fragment"), "{error}");
    let error = PeppyLauncherParser::from_content(&document(
        r#"{ name: "robot", default: "on", options: { on: {} } }"#,
    ))
    .unwrap_err()
    .to_string();
    assert!(error.contains(r#"{ robot: "<option>" }"#), "{error}");
    for properties in [
        "cardinality: 'many'",
        "cardinality: 'one_or_more'",
        "cardinality: 'zero_or_more', provides: null",
        "provides: null",
    ] {
        let axis = format!(r#"{{ name: "robot", {properties}, options: {{ on: {{}} }} }}"#);
        assert!(
            PeppyLauncherParser::from_content(&document(&axis)).is_err(),
            "{properties}"
        );
    }
}

/// A launcher adjustment guarded on a copy axis belongs to each copy of
/// that axis. The stack's report says so, and a copy applies it.
#[test]
fn an_adjustment_guarded_on_a_copy_axis_is_skipped_for_the_stack() {
    let document = |copies: &str| {
        format!(
            r#"{{
        peppy_schema: "launcher/v1",
        deployments: [
            {{ source: {{ name: "observer", tag: "v1" }},
              instances: [{{ instance_id: "observer_inst" }}] }},
            {copies}
        ],
        components: [{{ name: "robot", cardinality: "zero_or_more", options: {{
            sim: {{ deployments: [{{ source: {{ name: "arm", tag: "v1" }},
                instances: [{{ instance_id: "arm_inst" }}] }}] }}
        }} }}],
        adjustments: [{{ target: "observer_inst", when: {{ robot: "sim" }},
            set_arguments: {{ rate: 0.5 }} }}]
    }}"#
        )
    };
    let bare = load(&document("")).launch(&[]).unwrap();
    assert!(bare.copies().is_empty());
    let lines = bare.report.render_lines();
    assert!(
        lines.iter().any(|line| line
            == "  observer_inst: axis robot runs as copies; the adjustment runs in each copy  \
                (fleet.json5 (base))"),
        "{lines:?}"
    );
    assert!(
        !instance(&bare.launcher, "observer_inst")
            .arguments
            .contains_key("rate")
    );

    let with_copy = load(&document(
        r#"{ robot: "sim", instances: [{ instance_id: "alpha" }] }"#,
    ))
    .launch(&[])
    .unwrap();
    assert_eq!(
        instance(&with_copy.launcher, "observer_inst").arguments["rate"],
        AnyType::Float(0.5)
    );
}

/// A launcher adjustment on an id only a copy option defines belongs to
/// each copy of that axis, whether or not one runs.
#[test]
fn an_adjustment_on_an_id_only_copies_define_is_skipped_for_the_stack() {
    let document = |copies: &str| {
        format!(
            r#"{{
        peppy_schema: "launcher/v1",
        deployments: [
            {{ source: {{ name: "observer", tag: "v1" }},
              instances: [{{ instance_id: "observer_inst" }}] }},
            {copies}
        ],
        components: [{{ name: "robot", cardinality: "zero_or_more", options: {{
            sim: {{ deployments: [{{ source: {{ name: "arm", tag: "v1" }},
                instances: [{{ instance_id: "arm_inst" }}] }}] }}
        }} }}],
        adjustments: [{{ target: "arm_inst", set_arguments: {{ speed: 0.5 }} }}]
    }}"#
        )
    };
    let skipped = "  arm_inst: axis robot runs as copies; the adjustment runs in each copy  \
                   (fleet.json5 (base))";

    let bare = load(&document("")).launch(&[]).unwrap();
    assert_eq!(ids(&bare.launcher), ["observer_inst"]);
    let lines = bare.report.render_lines();
    assert!(lines.iter().any(|line| line == skipped), "{lines:?}");

    let with_copy = load(&document(
        r#"{ robot: "sim", instances: [{ instance_id: "alpha" }] }"#,
    ))
    .launch(&[])
    .unwrap();
    assert_eq!(
        instance(&with_copy.launcher, "alpha_arm_inst").arguments["speed"],
        AnyType::Float(0.5)
    );
    let lines = with_copy.report.render_lines();
    assert!(lines.iter().any(|line| line == skipped), "{lines:?}");
}

#[test]
fn minted_ids_never_collide_with_the_stack_or_with_each_other() {
    let prepared = load(
        r#"{
        peppy_schema: "launcher/v1",
        deployments: [{ source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "alpha_arm_inst" }] }],
        components: [{ name: "robot", cardinality: "zero_or_more", options: {
            real: { deployments: [{ source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] }] },
            wide: { deployments: [{ source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "b_arm_inst" }] }] }
        } }]
    }"#,
    );
    let launch = prepared.launch(&[]).unwrap();
    let error = join(
        &prepared,
        "real",
        "alpha",
        &launch.selection,
        &launch.launcher,
    )
    .unwrap_err();
    assert!(
        matches!(&error, CompositionError::PrefixedIdCollision { id, .. } if id == "alpha_arm_inst"),
        "{error}"
    );
    // `a` + `b_arm_inst` and `a_b` + `arm_inst` both mint `a_b_arm_inst`.
    let one = join(&prepared, "wide", "a", &launch.selection, &launch.launcher).unwrap();
    let error = join(&prepared, "real", "a_b", &launch.selection, &one).unwrap_err();
    assert!(
        matches!(&error, CompositionError::PrefixedIdCollision { copy, id }
            if copy == "a_b" && id == "a_b_arm_inst"),
        "{error}"
    );
    assert_eq!(ids(&launch.launcher), ["alpha_arm_inst"]);
}

#[test]
fn a_copy_cannot_place_itself() {
    let document = |copies: &str| {
        format!(
            r#"{{
            peppy_schema: "launcher/v1",
            core_nodes: ["cloud"],
            components: [{{ name: "robot", cardinality: "zero_or_more", options: {{
                real: {{ deployments: [{{ source: {{ name: "arm", tag: "v1" }}, instances: [
                    {{ instance_id: "arm_inst", core_node: "cloud" }}
                ] }}] }}
            }} }}],
            deployments: [{copies}]
        }}"#
        )
    };
    let prepared = load(&document(""));
    let launch = prepared.launch(&[]).unwrap();
    let error = join(
        &prepared,
        "real",
        "alpha",
        &launch.selection,
        &launch.launcher,
    )
    .unwrap_err();
    assert!(
        matches!(&error, CompositionError::CopyInstancePlaced { copy, instance, core_node }
            if copy == "alpha" && instance == "arm_inst" && core_node == "cloud"),
        "{error}"
    );
    let error = join(
        &prepared,
        "real",
        "cloud",
        &launch.selection,
        &launch.launcher,
    )
    .unwrap_err();
    assert!(
        matches!(&error, CompositionError::NameIsCoreNodeLink { name } if name == "cloud"),
        "{error}"
    );
    let error = PeppyLauncherParser::from_content(&document(
        r#"{ robot: "real", instances: [{ instance_id: "cloud" }] }"#,
    ))
    .unwrap_err()
    .to_string();
    assert!(error.contains("--place cloud@CORE_NODE"), "{error}");
}

#[test]
fn fragment_files_are_snapshotted_at_launch() {
    let directory = tempfile::tempdir().unwrap();
    let file = write(
        &directory.path().join("arm.json5"),
        &fragment_file(
            r#"deployments: [{ source: { name: "arm", tag: "v1" },
                instances: [{ instance_id: "arm_inst" }] }]"#,
        ),
    );
    let parsed = PeppyLauncherParser::from_content(
        r#"{ peppy_schema: "launcher/v1", components: [
            { name: "robot", cardinality: "zero_or_more", options: { real: "arm.json5" } }
        ] }"#,
    )
    .unwrap();
    let prepared = PreparedLauncher::load(&parsed, &directory.path().join("fleet.json5")).unwrap();
    let launch = prepared.launch(&[]).unwrap();
    std::fs::remove_file(file).unwrap();
    let joined = join(
        &prepared,
        "real",
        "alpha",
        &launch.selection,
        &launch.launcher,
    )
    .unwrap();
    assert_eq!(ids(&joined), ["alpha_arm_inst"]);
}

#[test]
fn every_link_follows_the_prefix_and_a_join_cannot_change_a_stack_instance() {
    let document = |copies: &str| {
        format!(
            r#"{{
            peppy_schema: "launcher/v1",
            deployments: [
                {{ source: {{ name: "observer", tag: "v1" }}, instances: [
                    {{ instance_id: "observer_inst", links: {{ robots: ["first"] }} }}
                ] }},
                {{ source: {{ name: "sensor", tag: "v1" }}, instances: [{{ instance_id: "first" }}] }},
                {copies}
            ],
            components: [
                {{ name: "robot", cardinality: "zero_or_more", options: {{
                    real: {{ deployments: [
                        {{ source: {{ name: "arm", tag: "v1" }}, instances: [{{ instance_id: "arm_inst" }}] }}
                    ] }}
                }} }},
                {{ name: "cameras", cardinality: "zero_or_more", options: {{
                    wrist: {{ deployments: [
                        {{ source: {{ name: "uvc", tag: "v1" }}, instances: [{{ instance_id: "wrist" }}] }}
                    ], adjustments: [
                        {{ target: "observer_inst", add_links: {{ robots: ["wrist"] }} }}
                    ] }}
                }} }}
            ]
        }}"#
        )
    };
    let prepared = load(&document(
        r#"{ robot: "real", instances: [{ instance_id: "alpha" }] },
           { cameras: "wrist", instances: [{ instance_id: "eye" }] }"#,
    ));
    let launch = prepared.launch(&[]).unwrap();
    assert_eq!(
        ids(&launch.launcher),
        ["observer_inst", "first", "alpha_arm_inst", "eye_wrist"]
    );
    // The stack-owned observer's link to the camera follows it under the prefix.
    assert_eq!(
        instance(&launch.launcher, "observer_inst").links["robots"]
            .selection()
            .unwrap()
            .targets(),
        &["first", "eye_wrist"]
    );
    assert_eq!(launch.copies()[1].selection.echo(), "cameras=wrist");
    // A later join cannot add itself to the running observer.
    let error = join(
        &prepared,
        "wrist",
        "iris",
        &launch.selection,
        &launch.launcher,
    )
    .unwrap_err();
    assert!(
        matches!(&error, CompositionError::JoinChangesExisting { name, instance, changes }
            if name == "iris" && instance == "observer_inst" && changes.contains("links.robots")),
        "{error}"
    );
    let joined = join(
        &prepared,
        "real",
        "bravo",
        &launch.selection,
        &launch.launcher,
    )
    .unwrap();
    assert_eq!(
        ids(&joined),
        [
            "observer_inst",
            "first",
            "alpha_arm_inst",
            "bravo_arm_inst",
            "eye_wrist"
        ]
    );
}

#[test]
fn copies_configure_stack_nodes_together_and_later_joins_cannot_change_them() {
    let document = |copies: &str| {
        format!(
            r#"{{
            peppy_schema: "launcher/v1",
            components: [
                {{ name: "simulation", provides: ["engine_inst"], options: {{
                    engine: {{ deployments: [{{ source: {{ name: "engine", tag: "v1" }}, instances: [
                        {{ instance_id: "engine_inst", arguments: {{ hardware: "v2" }} }}
                    ] }}] }}
                }} }},
                {{ name: "robot", cardinality: "zero_or_more", options: {{
                    v1: {{ deployments: [{{ source: {{ name: "arm", tag: "v1" }}, instances: [
                            {{ instance_id: "arm_inst", links: {{ engine: "engine_inst" }} }}
                        ] }}],
                        adjustments: [{{ target: "engine_inst", set_arguments: {{ hardware: "v1" }} }}] }},
                    v2: {{ deployments: [{{ source: {{ name: "arm", tag: "v1" }}, instances: [
                            {{ instance_id: "arm_inst", links: {{ engine: "engine_inst" }} }}
                        ] }}],
                        adjustments: [{{ target: "engine_inst", set_arguments: {{ hardware: "v2" }} }}] }}
                }} }}
            ],
            deployments: [{{ simulation: "engine" }}, {copies}]
        }}"#
        )
    };
    let prepared = load(&document(
        r#"{ robot: "v1", instances: [{ instance_id: "alpha" }] }"#,
    ));
    let launch = prepared.launch(&[]).unwrap();
    let engine = instance(&launch.launcher, "engine_inst");
    assert_eq!(engine.arguments["hardware"], AnyType::String("v1".into()));
    assert_eq!(launch.copies()[0].instance_ids, [name("alpha_arm_inst")]);
    let snapshot = serde_json::to_value(&launch.launcher).unwrap();
    let error = join(
        &prepared,
        "v2",
        "bravo",
        &launch.selection,
        &launch.launcher,
    )
    .unwrap_err();
    assert!(
        matches!(&error, CompositionError::JoinChangesExisting { instance, changes, .. }
            if instance == "engine_inst" && changes == "arguments.hardware: \"v1\" -> \"v2\""),
        "{error}"
    );
    assert_eq!(serde_json::to_value(&launch.launcher).unwrap(), snapshot);
    assert!(
        join(
            &prepared,
            "v1",
            "bravo",
            &launch.selection,
            &launch.launcher
        )
        .is_ok()
    );

    // Two copies deployed together must agree on the engine.
    let mixed = load(&document(
        r#"{ robot: "v1", instances: [{ instance_id: "alpha" }] },
           { robot: "v2", instances: [{ instance_id: "bravo" }] }"#,
    ));
    let error = mixed.launch(&[]).unwrap_err();
    assert!(
        matches!(&error, CompositionError::CopiesConflict { first, second, target, .. }
            if first == "alpha" && second == "bravo" && target == "engine_inst.arguments.hardware"),
        "{error}"
    );
    let agreed = load(&document(
        r#"{ robot: "v1", instances: [{ instance_id: "alpha" }, { instance_id: "bravo" }] }"#,
    ));
    assert_eq!(ids(&agreed.launch(&[]).unwrap().launcher).len(), 3);
}

#[test]
fn joins_preserve_stack_fragments_additive_adjustments() {
    let prepared = load(
        r#"{
        peppy_schema: "launcher/v1",
        deployments: [
            { source: { name: "observer", tag: "v1" }, instances: [
                { instance_id: "observer_inst", links: { sources: ["first"] } }
            ] },
            { source: { name: "sensor", tag: "v1" }, instances: [
                { instance_id: "first" }, { instance_id: "second" }
            ] },
            { robot: "real", instances: [{ instance_id: "alpha" }] }
        ],
        components: [
            { name: "simulation", cardinality: "zero_or_one", options: {
                engine: { adjustments: [
                    { target: "observer_inst", add_links: { sources: ["second"] } }
                ] }
            } },
            { name: "robot", cardinality: "zero_or_more", options: {
                real: { deployments: [
                    { source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] }
                ] }
            } }
        ]
    }"#,
    );
    let initial = prepared.launch(&words(&["engine"])).unwrap();
    let joined = join(
        &prepared,
        "real",
        "bravo",
        &initial.selection,
        &initial.launcher,
    )
    .unwrap();
    assert_eq!(
        instance(&joined, "observer_inst").links["sources"]
            .selection()
            .unwrap()
            .targets(),
        &["first", "second"]
    );
}

#[test]
fn the_repository_check_composes_every_launch_and_every_join() {
    let document = |engine_links: &str| {
        format!(
            r#"{{
            peppy_schema: "launcher/v1",
            components: [
                {{ name: "simulation", cardinality: "zero_or_one", options: {{
                    engine: {{ deployments: [
                        {{ source: {{ name: "engine", tag: "v1" }}, instances: [
                            {{ instance_id: "engine_inst"{engine_links} }}
                        ] }}
                    ] }}
                }} }},
                {{ name: "robot", cardinality: "zero_or_more", options: {{
                    real: {{
                        deployments: [{{ source: {{ name: "arm", tag: "v1" }}, instances: [
                            {{ instance_id: "arm_inst" }}
                        ] }}],
                        adjustments: [{{ target: "engine_inst", unset_links: ["obsolete"] }}]
                    }}
                }} }}
            ]
        }}"#
        )
    };
    // A copy repairs the engine's dangling link at launch; the stack alone
    // cannot, and the check reports it.
    let parsed =
        PeppyLauncherParser::from_content(&document(r#", links: { obsolete: "missing_inst" }"#))
            .unwrap();
    let problems = daemon_config::launcher::check_composition(&parsed, Path::new("fleet.json5"));
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("missing_inst")),
        "{problems:?}"
    );
    let sound = PeppyLauncherParser::from_content(&document("")).unwrap();
    let problems = daemon_config::launcher::check_composition(&sound, Path::new("fleet.json5"));
    assert!(problems.is_empty(), "{problems:?}");
}

/// A simulation declares the slot a robot's relay pairs into vacant, for
/// the launch without a robot; the relay's fragment drops the vacancy.
fn simulation_with_arm_slot(slot: &str) -> PreparedLauncher {
    load(&format!(
        r#"{{
        peppy_schema: "launcher/v1",
        components: [
            {{ name: "simulation", provides: ["simulation_inst"], options: {{
                mujoco: {{ deployments: [
                    {{ source: {{ name: "mujoco", tag: "v1" }}, instances: [
                        {{ instance_id: "simulation_inst", links: {{ arm: {slot} }} }}
                    ] }}
                ] }}
            }} }},
            {{ name: "robot", cardinality: "zero_or_more", options: {{
                sim: {{
                    deployments: [
                        {{ source: {{ name: "sim_arm", tag: "v1" }}, instances: [
                            {{ instance_id: "arm_inst", links: {{ engine: "simulation_inst/arm" }} }}
                        ] }}
                    ],
                    adjustments: [{{ target: "simulation_inst", unset_links: ["arm"] }}],
                }}
            }} }}
        ],
        deployments: [{{ simulation: "mujoco" }}]
    }}"#
    ))
}

#[test]
fn a_join_may_pair_into_a_slot_the_stack_declares_vacant() {
    let prepared = simulation_with_arm_slot(r#"{ vacant: "a simulated robot pairs here" }"#);
    let launch = prepared.launch(&[]).unwrap();
    assert!(
        instance(&launch.launcher, "simulation_inst")
            .links
            .contains_key("arm")
    );

    let joined = join(
        &prepared,
        "sim",
        "alpha",
        &launch.selection,
        &launch.launcher,
    )
    .unwrap();
    assert!(
        !instance(&joined, "simulation_inst")
            .links
            .contains_key("arm"),
        "the vacancy is released once a copy pairs into the slot"
    );
    assert_eq!(
        instance(&joined, "alpha_arm_inst").links["engine"],
        LinkValue::Bound(Selection::Scalar("simulation_inst/arm".into()))
    );
    // With the vacancy gone, the next copy over the same stack writes
    // nothing to the simulation.
    join(&prepared, "sim", "bravo", &launch.selection, &joined).unwrap();
}

#[test]
fn a_join_cannot_drop_a_slot_the_stack_binds() {
    let prepared = simulation_with_arm_slot(r#""simulation_inst""#);
    let launch = prepared.launch(&[]).unwrap();
    let err = join(
        &prepared,
        "sim",
        "alpha",
        &launch.selection,
        &launch.launcher,
    )
    .expect_err("a bound slot is what runs");
    assert!(
        matches!(&err, CompositionError::JoinChangesExisting { instance, changes, .. }
            if instance == "simulation_inst" && changes.contains("links.arm")),
        "got: {err}"
    );
}

/// Two copies write different fields of one stack instance: each field
/// has one writer, so they agree.
#[test]
fn copies_writing_different_fields_of_one_stack_instance_agree() {
    let prepared = load(
        r#"{
        peppy_schema: "launcher/v1",
        components: [
            { name: "simulation", options: { engine: { deployments: [
                { source: { name: "engine", tag: "v1" }, instances: [
                    { instance_id: "engine_inst", arguments: { hardware: "v2" } }
                ] }
            ] } } },
            { name: "robot", cardinality: "zero_or_more", options: {
                v1: { deployments: [{ source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] }],
                      adjustments: [{ target: "engine_inst", set_arguments: { hardware: "v1" } }] },
                tuner: { deployments: [{ source: { name: "tuner", tag: "v1" }, instances: [{ instance_id: "tuner_inst" }] }],
                         adjustments: [{ target: "engine_inst", set_arguments: { rate: 10 } }] }
            } }
        ],
        deployments: [
            { simulation: "engine" },
            { robot: "v1", instances: [{ instance_id: "alpha" }] },
            { robot: "tuner", instances: [{ instance_id: "bravo" }] }
        ]
    }"#,
    );
    let launch = prepared.launch(&[]).unwrap();
    let engine = instance(&launch.launcher, "engine_inst");
    assert_eq!(engine.arguments["hardware"], AnyType::String("v1".into()));
    assert_eq!(engine.arguments["rate"], AnyType::Int(10));

    // A join writes only its own fields against what runs.
    let joined = join(
        &prepared,
        "tuner",
        "charlie",
        &launch.selection,
        &launch.launcher,
    )
    .unwrap();
    assert_eq!(
        instance(&joined, "engine_inst").arguments["hardware"],
        AnyType::String("v1".into())
    );
}

/// Every copy of an option that registers itself with a stack observer
/// appends its own minted id; the appended set is the union.
#[test]
fn copies_appending_to_one_stack_slot_union_at_launch_and_cannot_append_at_join() {
    let prepared = load(
        r#"{
        peppy_schema: "launcher/v1",
        components: [
            { name: "cameras", cardinality: "zero_or_more", options: {
                wrist: { deployments: [{ source: { name: "camera", tag: "v1" }, instances: [{ instance_id: "wrist" }] }],
                         adjustments: [{ target: "observer_inst", add_links: { robots: ["wrist"] } }] }
            } }
        ],
        deployments: [
            { source: { name: "observer", tag: "v1" }, instances: [
                { instance_id: "observer_inst", links: { robots: ["first"] } }
            ] },
            { source: { name: "camera", tag: "v1" }, instances: [{ instance_id: "first" }] },
            { cameras: "wrist", instances: [{ instance_id: "eye" }, { instance_id: "iris" }] }
        ]
    }"#,
    );
    let launch = prepared.launch(&[]).unwrap();
    assert_eq!(
        instance(&launch.launcher, "observer_inst").links["robots"],
        LinkValue::Bound(Selection::Array(
            daemon_config::launcher::LinkTargets::new(vec![
                "first".into(),
                "eye_wrist".into(),
                "iris_wrist".into()
            ])
            .unwrap()
        ))
    );
    let error = join(
        &prepared,
        "wrist",
        "lens",
        &launch.selection,
        &launch.launcher,
    )
    .unwrap_err();
    assert!(
        matches!(&error, CompositionError::JoinChangesExisting { instance, changes, .. }
            if instance == "observer_inst" && changes == "links.robots: + lens_wrist"),
        "{error}"
    );
}

/// A launcher constraint naming a copy axis speaks for each copy of it;
/// the stack launches whether or not a copy runs.
#[test]
fn a_launcher_constraint_naming_a_copy_axis_speaks_for_its_copies() {
    let prepared = load(
        r#"{
        peppy_schema: "launcher/v1",
        components: [
            { name: "simulation", cardinality: "zero_or_one", options: { engine: { deployments: [
                { source: { name: "engine", tag: "v1" }, instances: [{ instance_id: "engine_inst" }] }
            ] } } },
            { name: "robot", cardinality: "zero_or_more", options: {
                sim: { deployments: [{ source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] }] },
                real: { deployments: [{ source: { name: "can_arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] }] }
            } }
        ],
        constraints: [
            { requires: [{ robot: "sim" }], reason: "this fleet simulates every robot" }
        ],
        deployments: []
    }"#,
    );
    let launch = prepared.launch(&[]).unwrap();
    join(
        &prepared,
        "sim",
        "alpha",
        &launch.selection,
        &launch.launcher,
    )
    .unwrap();
    let error = join(
        &prepared,
        "real",
        "bravo",
        &launch.selection,
        &launch.launcher,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("simulates every robot"),
        "{error}"
    );
}

/// An option's own axis cannot carry a launcher axis's name: its guards
/// would read the launcher's selection for their own.
#[test]
fn an_option_axis_cannot_shadow_a_launcher_axis() {
    let parsed = PeppyLauncherParser::from_content(
        r#"{
        peppy_schema: "launcher/v1",
        components: [
            { name: "simulation", options: { engine: { deployments: [
                { source: { name: "engine", tag: "v1" }, instances: [{ instance_id: "engine_inst" }] }
            ] } } },
            { name: "robot", cardinality: "zero_or_more", options: {
                sim: { deployments: [{ source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] },
                                     { simulation: "local" }],
                       components: [{ name: "simulation", options: { local: {} } }] }
            } }
        ],
        deployments: [{ simulation: "engine" }]
    }"#,
    )
    .unwrap();
    let error = PreparedLauncher::load(&parsed, Path::new("fleet.json5")).unwrap_err();
    assert!(
        matches!(&error, CompositionError::AxisInReachTwice { axis, first, .. }
            if axis == "simulation" && first == "the launcher"),
        "{error}"
    );
}

/// The options of the axes that run as copies are distinct, since
/// `stack join OPTION` names a copy by its option alone.
#[test]
fn copy_options_are_distinct_across_copy_axes() {
    let error = PeppyLauncherParser::from_content(
        r#"{
        peppy_schema: "launcher/v1",
        components: [
            { name: "robot", cardinality: "zero_or_more", options: { sim: {} } },
            { name: "cameras", cardinality: "zero_or_more", options: { sim: {} } }
        ],
        deployments: []
    }"#,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("`sim` is an option of both `robot` and `cameras`"),
        "{error}"
    );
}

/// A copy's name is its placement link, so `self` and an overlong name
/// are refused by name.
#[test]
fn a_copy_name_is_held_to_a_core_node_name() {
    let prepared = fleet("");
    let launch = prepared.launch(&words(&["engine"])).unwrap();
    for name in ["self", &"x".repeat(64)] {
        let error = join(&prepared, "sim", name, &launch.selection, &launch.launcher).unwrap_err();
        assert!(
            matches!(&error, CompositionError::CopyNameNotPlaceable { copy, .. } if copy == name),
            "{name}: {error}"
        );
    }
}

/// A copy's fragments define ids under the copy's name, so an id the stack
/// defines is refused by name.
#[test]
fn a_copy_cannot_reuse_an_id_the_stack_defines() {
    let prepared = load(
        r#"{
        peppy_schema: "launcher/v1",
        components: [
            { name: "robot", cardinality: "zero_or_more", options: {
                sim: { deployments: [{ source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "engine_inst" }] }] }
            } }
        ],
        deployments: [
            { source: { name: "engine", tag: "v1" }, instances: [{ instance_id: "engine_inst" }] }
        ]
    }"#,
    );
    let launch = prepared.launch(&[]).unwrap();
    let error = join(
        &prepared,
        "sim",
        "alpha",
        &launch.selection,
        &launch.launcher,
    )
    .unwrap_err();
    assert!(
        matches!(&error, CompositionError::CopyReusesStackId { option, id }
            if option == "sim" && id == "engine_inst"),
        "{error}"
    );
}

/// A stack fragment's adjustment guarded on a copy axis can never run.
#[test]
fn a_stack_fragment_cannot_guard_on_a_copy_axis() {
    let parsed = PeppyLauncherParser::from_content(
        r#"{
        peppy_schema: "launcher/v1",
        components: [
            { name: "simulation", options: { engine: {
                deployments: [{ source: { name: "engine", tag: "v1" }, instances: [{ instance_id: "engine_inst" }] }],
                adjustments: [{ target: "engine_inst", when: { robot: "sim" }, set_arguments: { fast: true } }]
            } } },
            { name: "robot", cardinality: "zero_or_more", options: {
                sim: { deployments: [{ source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] }] }
            } }
        ],
        deployments: [{ simulation: "engine" }]
    }"#,
    )
    .unwrap();
    let error = PreparedLauncher::load(&parsed, Path::new("fleet.json5")).unwrap_err();
    assert!(
        matches!(&error, CompositionError::GuardOnCopyAxis { target, axis, .. }
            if target == "engine_inst" && axis == "robot"),
        "{error}"
    );
}

/// Removing a copy leaves the stack as it runs; a stack instance that
/// links to the copy keeps it.
#[test]
fn a_copy_the_stack_links_to_cannot_be_removed() {
    let prepared = load(
        r#"{
        peppy_schema: "launcher/v1",
        components: [
            { name: "cameras", cardinality: "zero_or_more", options: {
                wrist: { deployments: [{ source: { name: "camera", tag: "v1" }, instances: [{ instance_id: "wrist" }] }],
                         adjustments: [{ target: "observer_inst", add_links: { robots: ["wrist"] } }] }
            } },
            { name: "robot", cardinality: "zero_or_more", options: {
                sim: { deployments: [{ source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] }] }
            } }
        ],
        deployments: [
            { source: { name: "observer", tag: "v1" }, instances: [{ instance_id: "observer_inst" }] },
            { cameras: "wrist", instances: [{ instance_id: "eye" }] },
            { robot: "sim", instances: [{ instance_id: "alpha" }] }
        ]
    }"#,
    );
    let launch = prepared.launch(&[]).unwrap();
    let eye = launch
        .copies()
        .iter()
        .find(|copy| copy.name == "eye")
        .unwrap();
    let error = prepared.remove(&launch.launcher, eye).unwrap_err();
    assert!(
        matches!(&error, CompositionError::CopyStillLinked { copy, links }
            if copy == "eye" && links == "observer_inst.robots -> eye_wrist"),
        "{error}"
    );
    let alpha = launch
        .copies()
        .iter()
        .find(|copy| copy.name == "alpha")
        .unwrap();
    let remaining = prepared.remove(&launch.launcher, alpha).unwrap();
    assert_eq!(ids(&remaining), ["observer_inst", "eye_wrist"]);
    assert!(!remaining.core_nodes.iter().any(|link| link == "alpha"));
}

fn load_error(document: &str) -> CompositionError {
    let parsed = PeppyLauncherParser::from_content(document).unwrap();
    PreparedLauncher::load(&parsed, Path::new("fleet.json5")).unwrap_err()
}

/// A launcher whose copies of `robot` may run beside copies of `cameras`,
/// with `extra` written into the `sim` robot option.
fn two_copy_axes(extra: &str, constraints: &str) -> String {
    format!(
        r#"{{
        peppy_schema: "launcher/v1",
        components: [
            {{ name: "simulation", cardinality: "zero_or_one", options: {{ engine: {{ deployments: [
                {{ source: {{ name: "engine", tag: "v1" }}, instances: [{{ instance_id: "engine_inst" }}] }}
            ] }} }} }},
            {{ name: "robot", cardinality: "zero_or_more", options: {{ sim: {{
                deployments: [{{ source: {{ name: "arm", tag: "v1" }}, instances: [{{ instance_id: "arm_inst" }}] }}],
                {extra}
            }} }} }},
            {{ name: "cameras", cardinality: "zero_or_more", options: {{ wrist: {{
                deployments: [{{ source: {{ name: "cam", tag: "v1" }}, instances: [{{ instance_id: "cam_inst" }}] }}],
            }} }} }},
        ],
        constraints: [{constraints}],
        deployments: [],
    }}"#
    )
}

/// One fragment file named by two options is one document to the check:
/// a constraint it carries is live when either option runs.
#[test]
fn a_fragment_file_shared_by_two_options_is_one_document_to_the_check() {
    let directory = tempfile::tempdir().unwrap();
    write(
        &directory.path().join("fragments/shared.json5"),
        &fragment_file(
            r#"deployments: [{ source: { name: "arm", tag: "v1" },
                instances: [{ instance_id: "arm_inst" }] }],
            constraints: [{ requires: [{ simulation: "engine" }],
                reason: "the arm is simulated" }]"#,
        ),
    );
    let launcher = write(
        &directory.path().join("fleet.json5"),
        r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "simulation", cardinality: "zero_or_one", options: { engine: { deployments: [
                    { source: { name: "engine", tag: "v1" }, instances: [{ instance_id: "engine_inst" }] }
                ] } } },
                { name: "robot", cardinality: "one", options: {
                    a: "fragments/shared.json5", b: "fragments/shared.json5",
                } },
            ],
            constraints: [{ when: { robot: "b" }, requires: [{ simulation: "engine" }], reason: "b is simulated" }],
            deployments: [{ robot: "a" }],
        }"#,
    );
    let parsed = PeppyLauncherParser::from_path(&launcher).unwrap();
    let problems = daemon_config::launcher::check_composition(&parsed, &launcher);
    assert!(
        problems
            .iter()
            .all(|problem| !problem.contains("refuses no selection")),
        "{problems:?}"
    );
}

/// A constraint or guard is refused where it names an axis its unit never
/// fills: a copy axis from a stack option's fragment, another copy axis
/// from a copy's fragment, two copy axes from the launcher.
#[test]
fn a_fragment_names_no_copy_axis_its_unit_cannot_fill() {
    let error = load_error(
        r#"{
        peppy_schema: "launcher/v1",
        components: [
            { name: "simulation", cardinality: "zero_or_one", options: { engine: {
                deployments: [{ source: { name: "engine", tag: "v1" }, instances: [{ instance_id: "engine_inst" }] }],
                constraints: [{ requires: [{ robot: "sim" }], reason: "the engine simulates a robot" }],
            } } },
            { name: "robot", cardinality: "zero_or_more", options: { sim: {} } },
        ],
        deployments: [],
    }"#,
    );
    assert!(
        matches!(&error, CompositionError::ConstraintOnCopyAxis { origin, axis }
            if origin == "inline option `simulation.engine`" && axis == "robot"),
        "{error}"
    );
    let error = load_error(&two_copy_axes(
        r#"constraints: [{ forbids: [{ cameras: "wrist" }], reason: "no cameras on a simulated arm" }],"#,
        "",
    ));
    assert!(
        matches!(&error, CompositionError::ConstraintOnCopyAxis { origin, axis }
            if origin == "inline option `robot.sim`" && axis == "cameras"),
        "{error}"
    );
    let error = load_error(&two_copy_axes(
        r#"adjustments: [{ target: "arm_inst", when: { cameras: "wrist" }, set_arguments: { filmed: true } }],"#,
        "",
    ));
    assert!(
        matches!(&error, CompositionError::GuardOnCopyAxis { axis, .. } if axis == "cameras"),
        "{error}"
    );
    let error = load_error(&two_copy_axes(
        "",
        r#"{ when: { robot: "sim" }, requires: [{ cameras: "wrist" }], reason: "a simulated arm is filmed" }"#,
    ));
    assert!(
        matches!(&error, CompositionError::ConstraintSpansCopyAxes { position: 1, axes }
            if axes.contains("`cameras`") && axes.contains("`robot`")),
        "{error}"
    );
}

/// A copy is placed by its name, so its fragments declare no core node
/// links.
#[test]
fn a_copy_fragment_declares_no_core_node_links() {
    let error = load_error(&two_copy_axes(r#"core_nodes: ["edge"],"#, ""));
    assert!(
        matches!(&error, CompositionError::CopyFragmentCoreNodes { origin, axis }
            if origin == "inline option `robot.sim`" && axis == "robot"),
        "{error}"
    );
}

/// A copy the file deploys overrides only instances of the options it
/// selects; the refusal lists those.
#[test]
fn a_file_copy_overrides_only_the_instances_it_selects() {
    let error = load_error(
        r#"{
        peppy_schema: "launcher/v1",
        components: [
            { name: "robot", cardinality: "zero_or_more", options: { real: {
                deployments: [
                    { source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] },
                    { commander: "web" },
                ],
                components: [{ name: "commander", options: {
                    web: { deployments: [{ source: { name: "web", tag: "v1" }, instances: [{ instance_id: "web_inst" }] }] },
                    xr: { deployments: [{ source: { name: "xr", tag: "v1" }, instances: [{ instance_id: "xr_inst" }] }] },
                } }],
            } } },
        ],
        deployments: [{ robot: "real", instances: [
            { instance_id: "alpha", with: { commander: "web" }, arguments: { xr_inst: { port: 1 } } }
        ] }],
    }"#,
    );
    assert!(
        matches!(&error, CompositionError::ArgumentTargetAbsent { copy, target, available }
            if copy == "alpha" && target == "xr_inst" && available == "`arm_inst`, `web_inst`"),
        "{error}"
    );
}

/// A launch word naming a copy axis with an unknown option is told the
/// axis runs as copies and shown its options; a join word naming nothing
/// the copied option declares is shown the option's own axes.
#[test]
fn copy_selection_refusals_name_the_copied_option() {
    let prepared = fleet("");
    let error = prepared.launch(&words(&["robot=nope"])).unwrap_err();
    assert!(
        matches!(&error, CompositionError::RepeatableAxisUnknownOption { word, axis, menu }
            if word == "robot=nope" && axis == "robot" && menu.contains("real")),
        "{error}"
    );
    assert!(
        error
            .to_string()
            .contains("peppy stack join OPTION -i NAME"),
        "{error}"
    );
    let launched = prepared.launch(&[]).unwrap();
    let error = prepared
        .join(
            JoinRequest {
                option: "real",
                name: &name("alpha"),
                words: &words(&["nope"]),
                arguments: &[],
            },
            RunningStack {
                selection: &launched.selection,
                launcher: &launched.launcher,
            },
        )
        .unwrap_err();
    assert!(
        matches!(&error, CompositionError::UnknownCopySelection { word, option, menu }
            if word == "nope" && option == "real" && menu.contains("commander")),
        "{error}"
    );
}

/// The stack unit runs only the launcher adjustments it can apply: one
/// targeting an id only copies define is reported skipped, naming the axis
/// whose copies run it. A write a copy makes to a stack instance is
/// reported under the copy's name.
#[test]
fn the_report_files_copy_writes_and_copy_only_adjustments_under_the_copy() {
    let prepared = load(
        r#"{
        peppy_schema: "launcher/v1",
        components: [
            { name: "simulation", cardinality: "one", options: { engine: { deployments: [
                { source: { name: "engine", tag: "v1" }, instances: [{ instance_id: "engine_inst", arguments: { hardware: "v2" } }] }
            ] } } },
            { name: "robot", cardinality: "zero_or_more", options: { v1: {
                deployments: [{ source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst", arguments: { speed: 1 } }] }],
                adjustments: [{ target: "engine_inst", set_arguments: { hardware: "v1" } }],
            } } },
        ],
        adjustments: [{ target: "arm_inst", set_arguments: { speed: 9 } }],
        deployments: [
            { simulation: "engine" },
            { robot: "v1", instances: [{ instance_id: "alpha" }] },
        ],
    }"#,
    );
    let launched = prepared.launch(&[]).unwrap();
    let report = &launched.report;
    assert!(
        matches!(
            report.skipped.as_slice(),
            [SkippedAdjustment { target, reason: SkipReason::RunsInCopies(axis), .. }]
                if target == "arm_inst" && axis == "robot"
        ),
        "{:?}",
        report.skipped
    );
    let speed = report
        .applied
        .iter()
        .find(|entry| entry.target == "alpha_arm_inst")
        .expect("the copy runs the launcher's adjustment");
    assert_eq!(speed.change.field(), "arguments.speed");
    let hardware = report
        .applied
        .iter()
        .find(|entry| entry.target == "engine_inst")
        .expect("the copy writes the engine's hardware");
    assert!(
        hardware.origin.ends_with(", copy `alpha`"),
        "{}",
        hardware.origin
    );
}

/// A copy pairs into a vacant stack slot from its own instance; a copy
/// fragment binding the slot from the stack side is told the spelling.
#[test]
fn binding_a_vacant_slot_from_the_stack_side_is_refused_with_the_spelling() {
    let prepared = load(
        r#"{
        peppy_schema: "launcher/v1",
        components: [
            { name: "simulation", cardinality: "one", options: { engine: { deployments: [
                { source: { name: "engine", tag: "v1" }, instances: [
                    { instance_id: "engine_inst", links: { arm: { vacant: "a simulated arm pairs here" } } }
                ] }
            ] } } },
            { name: "robot", cardinality: "zero_or_more", options: { sim: {
                deployments: [{ source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] }],
                adjustments: [{ target: "engine_inst", set_links: { arm: "arm_inst" } }],
            } } },
        ],
        deployments: [{ simulation: "engine" }],
    }"#,
    );
    let launched = prepared.launch(&[]).unwrap();
    let error = join(
        &prepared,
        "sim",
        "alpha",
        &launched.selection,
        &launched.launcher,
    )
    .unwrap_err();
    assert!(
        matches!(&error, CompositionError::JoinChangesExisting { changes, .. }
            if changes.contains("links.arm is vacant; pair into it from the copy's own instance")),
        "{error}"
    );
}

/// An option entry's own `with` and `arguments` apply to every copy it
/// lists; a copy's own values win per axis and per argument. An entry
/// carrying them without copies is refused.
#[test]
fn an_option_entry_shares_its_settings_with_the_copies_it_lists() {
    let prepared = fleet(
        r#"{ robot: "real", with: { commander: "xr" }, arguments: { arm_inst: { speed: 0.75 } },
            instances: [
              { instance_id: "alpha" },
              { instance_id: "bravo", with: { commander: "web" }, arguments: { arm_inst: { speed: 0.9 } } },
            ] }"#,
    );
    let launched = prepared.launch(&[]).unwrap();
    assert_eq!(
        launched
            .copies()
            .iter()
            .map(|copy| copy.selection.echo())
            .collect::<Vec<_>>(),
        ["robot=real  commander=xr", "robot=real  commander=web"]
    );
    assert_eq!(
        instance(&launched.launcher, "alpha_arm_inst").arguments["speed"],
        AnyType::Float(0.75)
    );
    assert_eq!(
        instance(&launched.launcher, "bravo_arm_inst").arguments["speed"],
        AnyType::Float(0.9)
    );
    assert_eq!(
        ids(&launched.launcher)
            .iter()
            .filter(|id| id.ends_with("commander_inst"))
            .count(),
        2
    );
    assert!(
        instance(&launched.launcher, "alpha_commander_inst")
            .links
            .contains_key("arm")
    );

    let error = PeppyLauncherParser::from_content(
        r#"{
        peppy_schema: "launcher/v1",
        components: [{ name: "robot", cardinality: "zero_or_more", options: { real: {} } }],
        deployments: [{ robot: "real", with: { commander: "xr" } }],
    }"#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("runs as named copies"), "{error}");
}

/// An entry's and a copy's own `adjustments` write to the copy's instances
/// after the launcher's adjustments and before its `arguments`, guarded by
/// the copy's selection; a target no option of the copy defines is refused.
#[test]
fn copy_adjustments_run_after_the_launchers_and_before_the_copys_arguments() {
    let prepared = fleet(
        r#"{ robot: "sim",
            adjustments: [{ target: "arm_inst", set_arguments: { speed: 0.6 } }],
            arguments: { arm_inst: { torque: 3 } },
            instances: [
              { instance_id: "alpha" },
              { instance_id: "bravo",
                adjustments: [
                  { target: "commander_inst", when: { commander: "xr" }, set_arguments: { https_port: 4444 } },
                  { target: "arm_inst", set_arguments: { speed: 0.7 } },
                ],
                arguments: { arm_inst: { speed: 0.8 } } },
            ] }"#,
    );
    let launched = prepared.launch(&words(&["engine"])).unwrap();
    // The entry's argument survives beside the copy's own key.
    assert_eq!(
        instance(&launched.launcher, "bravo_arm_inst").arguments["torque"],
        AnyType::Int(3)
    );
    // The launcher's guarded adjustment sets 0.5, the entry's 0.6 on top.
    assert_eq!(
        instance(&launched.launcher, "alpha_arm_inst").arguments["speed"],
        AnyType::Float(0.6)
    );
    // The copy's adjustment sets 0.7, its `arguments` 0.8 last.
    assert_eq!(
        instance(&launched.launcher, "bravo_arm_inst").arguments["speed"],
        AnyType::Float(0.8)
    );
    assert!(
        !instance(&launched.launcher, "bravo_commander_inst")
            .arguments
            .contains_key("https_port"),
        "the guard on the copy's commander does not hold for the web commander"
    );
    let skipped: Vec<_> = launched
        .report
        .skipped
        .iter()
        .map(|entry| (entry.target.as_str(), entry.origin.as_str()))
        .collect();
    assert!(
        skipped.contains(&("bravo_commander_inst", "adjustments of copy `bravo`")),
        "{skipped:?}"
    );
    let origins = |target: &str| -> Vec<String> {
        launched
            .report
            .applied
            .iter()
            .filter(|entry| entry.target == target)
            .map(|entry| entry.origin.clone())
            .collect()
    };
    assert_eq!(
        origins("alpha_arm_inst"),
        [
            "fleet.json5 (base)",
            "adjustments of `robot: sim`",
            "arguments of copy `alpha`",
        ]
    );
    assert_eq!(
        origins("bravo_arm_inst"),
        [
            "fleet.json5 (base)",
            "adjustments of `robot: sim`",
            "adjustments of copy `bravo`",
            "arguments of copy `bravo`",
            "arguments of copy `bravo`",
        ]
    );

    let error = load_error(&fleet_document(
        r#"{ robot: "sim", instances: [
            { instance_id: "alpha", adjustments: [{ target: "nowhere_inst", set_arguments: { x: 1 } }] },
        ] }"#,
    ));
    assert!(
        matches!(&error, CompositionError::CopyAdjustmentTarget { target, origin, .. }
            if target == "nowhere_inst" && origin == "adjustments of copy `alpha`"),
        "{error}"
    );
}

/// A launch word `NAME.axis=option` or `NAME.option` selects one file
/// copy's own axis for that launch, over the file's `with`; a word naming
/// a copy the file does not deploy is refused with the file's copies.
#[test]
fn a_launch_word_selects_a_file_copys_own_axis() {
    let prepared = fleet(
        r#"{ robot: "real", instances: [
            { instance_id: "alpha", with: { commander: "web" } },
            { instance_id: "bravo" },
        ] }"#,
    );
    let launched = prepared.launch(&words(&["alpha.xr"])).unwrap();
    assert_eq!(
        launched
            .copies()
            .iter()
            .map(|copy| copy.selection.echo())
            .collect::<Vec<_>>(),
        [
            "robot=real  commander=xr",
            "robot=real  commander=web (from file)"
        ]
    );
    let launched = prepared
        .launch(&words(&["bravo.commander=xr", "engine"]))
        .unwrap();
    assert_eq!(
        launched
            .copies()
            .iter()
            .map(|copy| copy.selection.echo())
            .collect::<Vec<_>>(),
        ["robot=real  commander=web", "robot=real  commander=xr"]
    );
    assert!(ids(&launched.launcher).contains(&"engine_inst".to_owned()));

    let error = prepared.launch(&words(&["charlie.xr"])).unwrap_err();
    assert!(
        matches!(&error, CompositionError::ScopedSelectionUnknownCopy { word, copy, copies }
            if word == "charlie.xr" && copy == "charlie"
                && copies.starts_with("the file deploys `alpha`, `bravo`")),
        "{error}"
    );
    let error = prepared.launch(&words(&["alpha.nope"])).unwrap_err();
    assert!(
        matches!(&error, CompositionError::UnknownCopySelection { word, option, .. }
            if word == "alpha.nope" && option == "real"),
        "{error}"
    );
}

/// Refusals of copy adjustments and scoped words: conflicting words for one
/// copy, a guard on a copy axis, a stack instance as target, a word with no
/// copy before the dot, and settings on a `one` axis entry.
#[test]
fn copy_adjustment_and_scoped_word_refusals_name_the_fix() {
    let prepared = fleet(r#"{ robot: "real", instances: [{ instance_id: "alpha" }] }"#);
    let error = prepared
        .launch(&words(&["alpha.xr", "alpha.commander=web"]))
        .unwrap_err();
    assert!(
        matches!(&error, CompositionError::ConflictingSelection { .. }),
        "{error}"
    );
    let error = prepared.launch(&words(&[".xr"])).unwrap_err();
    assert!(
        matches!(&error, CompositionError::ScopedWordNamesNoCopy { word } if word == ".xr"),
        "{error}"
    );
    let error = prepared.launch(&words(&["alpha."])).unwrap_err();
    assert!(
        matches!(&error, CompositionError::ScopedWordNamesNoOption { word, copy }
            if word == "alpha." && copy == "alpha"),
        "{error}"
    );

    let error = load_error(&fleet_document(
        r#"{ robot: "real", instances: [{ instance_id: "alpha",
            adjustments: [{ target: "arm_inst", when: { robot: "sim" }, set_arguments: { speed: 1 } }] }] }"#,
    ));
    assert!(
        matches!(&error, CompositionError::CopyAdjustmentOnCopyAxis { origin, axis }
            if origin == "adjustments of copy `alpha`" && axis == "robot"),
        "{error}"
    );
    let error = load_error(&fleet_document(
        r#"{ robot: "sim", adjustments: [{ target: "engine_inst", set_arguments: { hardware: "v1" } }],
            instances: [{ instance_id: "alpha" }] }"#,
    ));
    assert!(
        matches!(&error, CompositionError::CopyAdjustmentTarget { origin, target, available }
            if origin == "adjustments of `robot: sim`" && target == "engine_inst"
                && available == "`arm_inst`, `commander_inst`"),
        "{error}"
    );
    let error = load_error(&fleet_document(
        r#"{ robot: "real", instances: [{ instance_id: "alpha",
            adjustments: [{ target: "arm_inst", when: { nope: "x" }, set_arguments: { speed: 1 } }] }] }"#,
    ));
    assert!(
        matches!(&error, CompositionError::CopyAdjustmentGuard { origin, .. }
            if origin == "adjustments of copy `alpha`"),
        "{error}"
    );

    let error = PeppyLauncherParser::from_content(
        r#"{
        peppy_schema: "launcher/v1",
        components: [{ name: "simulation", options: { engine: {} } }],
        deployments: [{ simulation: "engine", with: { x: "y" } }],
    }"#,
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("takes no `instances`, `with`, `arguments` or `adjustments`"),
        "{error}"
    );
}
