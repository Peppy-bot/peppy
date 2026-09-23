use clap::Parser;
use config::runtime::{CoreNodeName, Name};
use core_node_api::encoding::JoinPlacement;
use daemon_config::consts::PeppyDirs;
use peppy::commands::stack::{JoinPreview, LauncherArgs, StackCommands, resolve_rendered};

#[derive(Parser)]
struct StackCli {
    #[command(subcommand)]
    command: StackCommands,
}

#[test]
fn stack_join_cli_names_the_copy_and_accepts_placement_overrides_and_timeouts() {
    let cli = StackCli::try_parse_from([
        "stack",
        "join",
        "real:alpha",
        "--with",
        "xr",
        "--set-arguments",
        "arm_inst.speed=0.2",
        "--place",
        "jetson-1",
        "--node-build-idle-timeout-secs",
        "900",
    ])
    .unwrap();
    let StackCommands::Join {
        copy,
        with,
        arguments,
        place,
        timeouts,
        ..
    } = cli.command
    else {
        panic!("join")
    };
    assert_eq!(copy.option, "real");
    assert_eq!(copy.name.as_str(), "alpha");
    assert_eq!(with.words, ["xr"]);
    assert_eq!(arguments, ["arm_inst.speed=0.2".parse().unwrap()]);
    assert_eq!(
        place,
        Some(JoinPlacement::CoreNode(
            CoreNodeName::new("jetson-1").unwrap()
        ))
    );
    assert_eq!(timeouts.node_build_idle_timeout_secs, 900);
    for args in [
        vec!["stack", "join"],
        vec!["stack", "join", "real"],
        vec!["stack", "join", ":alpha"],
        vec!["stack", "join", "real:"],
        vec!["stack", "join", "real:bad/name"],
        vec!["stack", "join", "real:self"],
        vec!["stack", "join", "real", "-i", "alpha"],
        vec!["stack", "resolve", "fleet", "--then-join-with", "xr"],
        vec!["stack", "resolve", "fleet", "--then-join-name", "alpha"],
        vec!["stack", "launch", "fleet", "--join", "real"],
        vec!["stack", "launch", "fleet", "--join", ":alpha"],
        vec!["stack", "launch", "fleet", "--join", "real:bad/name"],
        vec!["stack", "launch", "fleet", "--join", "real:self"],
        vec!["stack", "resolve", "fleet", "--join", "real"],
        vec!["stack", "launch", "fleet", "-i", "alpha"],
        vec![
            "stack",
            "launch",
            "fleet",
            "--set-arguments",
            "arm_inst.speed=1",
        ],
        vec![
            "stack",
            "resolve",
            "fleet",
            "--set-arguments",
            "arm_inst.speed=1",
        ],
    ] {
        assert!(StackCli::try_parse_from(&args).is_err(), "{args:?}");
    }
    // On launch `--place` wires a link to a machine; on join it names the
    // machine, the copy's name being its one link.
    let error = StackCli::try_parse_from(["stack", "launch", "fleet", "--place", "alpha"])
        .err()
        .expect("--place on launch needs a link and a core node")
        .to_string();
    assert!(error.contains("expected NAME@CORE_NODE"), "{error}");
    let error =
        StackCli::try_parse_from(["stack", "join", "real:alpha", "--place", "alpha@jetson-1"])
            .err()
            .expect("--place on join names a machine")
            .to_string();
    assert!(error.contains("`--place jetson-1`"), "{error}");
}

#[test]
fn stack_join_and_preview_reject_nonfinite_overrides() {
    for value in [
        "NaN",
        "Infinity",
        "-Infinity",
        "[0, NaN]",
        "{speed: Infinity}",
    ] {
        let argument = format!("arm_inst.speed={value}");
        for prefix in [
            vec!["stack", "join", "real:alpha"],
            vec!["stack", "resolve", "fleet", "--then-join", "real"],
        ] {
            let flag = if prefix.contains(&"resolve") {
                "--then-join-set-arguments"
            } else {
                "--set-arguments"
            };
            let args: Vec<_> = prefix
                .into_iter()
                .chain([flag, argument.as_str()])
                .collect();
            let error = StackCli::try_parse_from(args)
                .err()
                .expect("non-finite override is rejected");
            assert!(error.to_string().contains("finite"), "{error}");
        }
    }
}

/// A launch word scoped to a file copy, `NAME.axis=option`, fills that
/// copy's own axis through the CLI's resolve path.
#[test]
fn stack_resolve_applies_a_launch_word_scoped_to_a_file_copy() {
    let directory = tempfile::tempdir().unwrap();
    let launcher = directory.path().join("simulation.json5");
    std::fs::write(&launcher, r#"{
        peppy_schema: "launcher/v1",
        components: [{ name: "robot", cardinality: "zero_or_more", options: { real: {
            deployments: [
                { source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] },
                { commander: "web" },
            ],
            components: [{ name: "commander", options: {
                web: { deployments: [{ source: { name: "panel", tag: "v1" }, instances: [{ instance_id: "commander_inst" }] }] },
                xr: { deployments: [{ source: { name: "headset", tag: "v1" }, instances: [{ instance_id: "commander_inst" }] }] },
            } }],
        } } }],
        deployments: [{ robot: "real", instances: [{ instance_id: "alpha" }] }],
    }"#).unwrap();
    let no_join = JoinPreview {
        option: None,
        name: Name::new("unused").unwrap(),
        words: Vec::new(),
        arguments: Vec::new(),
    };
    let (document, report) = resolve_rendered(
        &PeppyDirs::new(directory.path()),
        launcher,
        &["alpha.commander=xr".into()],
        &[],
        &no_join,
    )
    .unwrap();
    assert!(
        report
            .iter()
            .any(|line| line == "copy alpha: robot=real  commander=xr"),
        "{report:?}"
    );
    let flat: serde_json::Value = serde_json5::from_str(&document).unwrap();
    let sources: Vec<_> = flat["deployments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|deployment| deployment["source"]["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(sources.contains(&"headset".to_owned()), "{sources:?}");
    assert!(!sources.contains(&"panel".to_owned()), "{sources:?}");
}

#[test]
fn stack_resolve_join_uses_shared_state_prefixes_and_override_precedence() {
    let directory = tempfile::tempdir().unwrap();
    let launcher = directory.path().join("fleet.json5");
    std::fs::write(&launcher, r#"{
        peppy_schema: "launcher/v1",
        components: [{ name: "simulation", cardinality: "zero_or_one", options: { sim: {
                deployments: [{ source: { name: "engine", tag: "v1" }, instances: [{ instance_id: "engine_inst" }] }]
            }}},
            { name: "robot", cardinality: "zero_or_more", options: { real: {
                deployments: [{ source: { name: "arm", tag: "v1" }, instances: [
                    { instance_id: "arm_inst", arguments: { speed: 0.1 }, links: { engine: "engine_inst" } }
                ] }]
            }}}
        ],
        adjustments: [{ target: "arm_inst", set_arguments: { speed: 0.5 } }]
    }"#).unwrap();
    let join = JoinPreview {
        option: Some("real".into()),
        name: Name::new("alpha").unwrap(),
        words: Vec::new(),
        arguments: vec!["arm_inst.speed=0.2".parse().unwrap()],
    };
    let (document, report) = resolve_rendered(
        &PeppyDirs::new(directory.path()),
        launcher.clone(),
        &["sim".into()],
        &[],
        &join,
    )
    .unwrap();
    let flat: serde_json::Value = serde_json5::from_str(&document).unwrap();
    assert_eq!(flat["core_nodes"], serde_json::json!(["alpha"]));
    let instances: Vec<_> = flat["deployments"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|deployment| deployment["instances"].as_array().unwrap())
        .collect();
    assert_eq!(instances.len(), 2);
    let arm = instances
        .iter()
        .find(|instance| instance["instance_id"] == "alpha_arm_inst")
        .unwrap();
    assert_eq!(arm["arguments"]["speed"], 0.2);
    assert_eq!(arm["links"]["engine"], "engine_inst");
    assert_eq!(arm["core_node"], "alpha");
    assert!(
        report
            .iter()
            .any(|line| line == "copy `alpha` of `real` joined:"),
        "{report:?}"
    );
    // A copy is never a launch word.
    let error = resolve_rendered(
        &PeppyDirs::new(directory.path()),
        launcher,
        &["real".into()],
        &[],
        &JoinPreview::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("peppy stack join real:NAME"), "{error}");
}

/// `stack resolve --then-join` previews what `stack join` composes: the copy
/// starts from the launcher's entry for the option, a `--then-join-with` word
/// wins on its axis, and `--then-join-set-arguments` wins per argument.
#[test]
fn stack_resolve_join_starts_from_the_launchers_entry_for_the_option() {
    let directory = tempfile::tempdir().unwrap();
    let launcher = directory.path().join("fleet.json5");
    std::fs::write(&launcher, r#"{
        peppy_schema: "launcher/v1",
        components: [{ name: "robot", cardinality: "zero_or_more", options: { real: {
            deployments: [
                { source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst", arguments: { speed: 0.1 } }] },
                { commander: "web" },
            ],
            components: [{ name: "commander", options: {
                web: { deployments: [{ source: { name: "panel", tag: "v1" }, instances: [{ instance_id: "commander_inst", arguments: { port: 8765 } }] }] },
                mcp: { deployments: [{ source: { name: "mcp", tag: "v1" }, instances: [{ instance_id: "commander_inst", arguments: { port: 8900 } }] }] },
            } }],
        } } }],
        deployments: [{ robot: "real",
            with: { commander: "mcp" },
            arguments: { arm_inst: { speed: 0.5 }, commander_inst: { port: 9000 } },
            instances: [{ instance_id: "alpha" }] }],
    }"#).unwrap();
    let resolve = |words: &[&str], arguments: &[&str]| {
        let join = JoinPreview {
            option: Some("real".into()),
            name: Name::new("bravo").unwrap(),
            words: words.iter().map(|word| word.to_string()).collect(),
            arguments: arguments
                .iter()
                .map(|argument| argument.parse().unwrap())
                .collect(),
        };
        let (document, report) = resolve_rendered(
            &PeppyDirs::new(directory.path()),
            launcher.clone(),
            &[],
            &[],
            &join,
        )
        .unwrap();
        let flat: serde_json::Value = serde_json5::from_str(&document).unwrap();
        (flat, report)
    };
    let instance = |flat: &serde_json::Value, id: &str| -> serde_json::Value {
        flat["deployments"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|deployment| deployment["instances"].as_array().unwrap())
            .find(|instance| instance["instance_id"] == id)
            .unwrap_or_else(|| panic!("{id} is in the plan: {flat}"))
            .clone()
    };
    let source_of = |flat: &serde_json::Value, id: &str| -> String {
        flat["deployments"]
            .as_array()
            .unwrap()
            .iter()
            .find(|deployment| {
                deployment["instances"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|instance| instance["instance_id"] == id)
            })
            .unwrap_or_else(|| panic!("{id} is in the plan: {flat}"))["source"]["name"]
            .as_str()
            .unwrap()
            .to_owned()
    };

    let (flat, report) = resolve(&[], &[]);
    assert!(
        report
            .iter()
            .any(|line| line == "copy bravo: robot=real  commander=mcp"),
        "{report:?}"
    );
    assert_eq!(source_of(&flat, "bravo_commander_inst"), "mcp");
    assert_eq!(
        instance(&flat, "bravo_commander_inst")["arguments"],
        instance(&flat, "alpha_commander_inst")["arguments"]
    );
    assert_eq!(instance(&flat, "bravo_arm_inst")["arguments"]["speed"], 0.5);

    let (flat, _) = resolve(&["web"], &["commander_inst.port=8910"]);
    assert_eq!(source_of(&flat, "bravo_commander_inst"), "panel");
    assert_eq!(
        instance(&flat, "bravo_commander_inst")["arguments"]["port"],
        8910
    );
    assert_eq!(
        instance(&flat, "bravo_arm_inst")["arguments"]["speed"],
        0.5,
        "the entry's other arguments stand"
    );
    assert_eq!(
        instance(&flat, "alpha_commander_inst")["arguments"]["port"],
        9000
    );
}

#[test]
fn resolve_previews_a_join_with_its_own_name_selection_and_overrides() {
    let cli = StackCli::try_parse_from([
        "stack",
        "resolve",
        "fleet",
        "--with",
        "mujoco",
        "--then-join",
        "openarm_v2_sim",
        "--then-join-name",
        "bravo",
        "--then-join-with",
        "xr_commander",
        "--then-join-set-arguments",
        "commander_inst.port=8001",
    ])
    .unwrap();
    let StackCommands::Resolve {
        with, then_join, ..
    } = cli.command
    else {
        unreachable!()
    };
    assert_eq!(with.words, ["mujoco"]);
    assert_eq!(then_join.option.as_deref(), Some("openarm_v2_sim"));
    assert_eq!(then_join.name.as_str(), "bravo");
    assert_eq!(then_join.words, ["xr_commander"]);
    assert_eq!(
        then_join.arguments,
        ["commander_inst.port=8001".parse().unwrap()]
    );
    let cli =
        StackCli::try_parse_from(["stack", "resolve", "fleet", "--then-join", "openarm_v2_sim"])
            .unwrap();
    let StackCommands::Resolve { then_join, .. } = cli.command else {
        unreachable!()
    };
    assert_eq!(then_join.name.as_str(), "preview");
}

#[test]
fn a_launch_time_join_names_an_option_and_a_copy() {
    let cli = StackCli::try_parse_from([
        "stack",
        "launch",
        "fleet",
        "--join",
        "openarm_v2_sim:alpha",
        "--join",
        "so101_sim:charlie",
        "--with",
        "alpha.xr_commander",
    ])
    .unwrap();
    let StackCommands::Launch(LauncherArgs { joins, with, .. }) = cli.command else {
        unreachable!()
    };
    assert_eq!(
        joins
            .joins
            .iter()
            .map(|join| (join.option.as_str(), join.name.as_str()))
            .collect::<Vec<_>>(),
        [("openarm_v2_sim", "alpha"), ("so101_sim", "charlie")]
    );
    assert_eq!(with.words, ["alpha.xr_commander"]);
    let cli = StackCli::try_parse_from([
        "stack",
        "resolve",
        "fleet",
        "--join",
        "openarm_v2_sim:alpha",
        "--then-join",
        "so101_sim",
    ])
    .unwrap();
    let StackCommands::Resolve {
        joins, then_join, ..
    } = cli.command
    else {
        unreachable!()
    };
    assert_eq!(joins.joins[0].name.as_str(), "alpha");
    assert_eq!(then_join.option.as_deref(), Some("so101_sim"));
    let error = StackCli::try_parse_from(["stack", "launch", "fleet", "--join", "real"])
        .err()
        .expect("a launch-time join names a copy")
        .to_string();
    assert!(error.contains("write `OPTION:NAME`"), "{error}");
}

/// `--join` takes several copies comma-separated as well as repeated, the
/// way `--with` takes several words, and keeps the order they are typed in.
#[test]
fn a_launch_time_join_takes_several_copies_in_one_word() {
    let cli = StackCli::try_parse_from([
        "stack",
        "build",
        "fleet",
        "--join",
        "openarm_v2_sim:alpha,so101_sim:charlie",
        "--join",
        "openarm_v1_sim:bravo",
    ])
    .unwrap();
    let StackCommands::Build(LauncherArgs { joins, .. }) = cli.command else {
        unreachable!()
    };
    assert_eq!(
        joins
            .joins
            .iter()
            .map(|join| (join.option.as_str(), join.name.as_str()))
            .collect::<Vec<_>>(),
        [
            ("openarm_v2_sim", "alpha"),
            ("so101_sim", "charlie"),
            ("openarm_v1_sim", "bravo"),
        ]
    );
}

#[test]
fn resolve_starts_the_copies_a_launch_time_join_names_from_the_options_entry() {
    let directory = tempfile::tempdir().unwrap();
    let launcher = directory.path().join("fleet.json5");
    std::fs::write(&launcher, r#"{
        peppy_schema: "launcher/v1",
        components: [{ name: "robot", cardinality: "zero_or_more", options: { real: {
            deployments: [
                { source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] },
                { commander: "web" },
            ],
            components: [{ name: "commander", provides: ["commander_inst"], options: {
                web: { deployments: [{ source: { name: "panel", tag: "v1" }, instances: [{ instance_id: "commander_inst" }] }] },
                xr: { deployments: [{ source: { name: "headset", tag: "v1" }, instances: [{ instance_id: "commander_inst" }] }] },
            } }],
        } } }],
        deployments: [{ robot: "real", with: { commander: "xr" } }],
    }"#).unwrap();
    let joins = |names: &[&str]| -> Vec<core_node_api::encoding::LaunchJoin> {
        names
            .iter()
            .map(|name| core_node_api::encoding::LaunchJoin {
                option: "real".into(),
                name: Name::new(*name).unwrap(),
            })
            .collect()
    };
    // The entry lists no copy, so a bare launch starts none.
    let (document, report) = resolve_rendered(
        &PeppyDirs::new(directory.path()),
        launcher.clone(),
        &[],
        &[],
        &JoinPreview::default(),
    )
    .unwrap();
    assert!(
        !report.iter().any(|line| line.starts_with("copy ")),
        "{report:?}"
    );
    let flat: serde_json::Value = serde_json5::from_str(&document).unwrap();
    assert!(
        flat["deployments"].as_array().is_none_or(Vec::is_empty),
        "{flat}"
    );

    // Two joins at launch: each a copy of the entry, under the
    // entry's `with`, and a scoped word selects one copy's own axis.
    let (document, report) = resolve_rendered(
        &PeppyDirs::new(directory.path()),
        launcher.clone(),
        &["bravo.commander=web".into()],
        &joins(&["alpha", "bravo"]),
        &JoinPreview::default(),
    )
    .unwrap();
    assert!(
        report
            .iter()
            .any(|line| line == "copy alpha: robot=real  commander=xr"),
        "{report:?}"
    );
    assert!(
        report
            .iter()
            .any(|line| line == "copy bravo: robot=real  commander=web"),
        "{report:?}"
    );
    let flat: serde_json::Value = serde_json5::from_str(&document).unwrap();
    assert_eq!(flat["core_nodes"], serde_json::json!(["alpha", "bravo"]));
    let sources: Vec<(String, String)> = flat["deployments"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|deployment| {
            let source = deployment["source"]["name"].as_str().unwrap().to_owned();
            deployment["instances"]
                .as_array()
                .unwrap()
                .iter()
                .map(move |instance| {
                    (
                        source.clone(),
                        instance["instance_id"].as_str().unwrap().to_owned(),
                    )
                })
        })
        .collect();
    assert!(
        sources.contains(&("headset".into(), "alpha_commander_inst".into())),
        "{sources:?}"
    );
    assert!(
        sources.contains(&("panel".into(), "bravo_commander_inst".into())),
        "{sources:?}"
    );

    // A name the launch already starts is refused, and so is an option no
    // repeatable axis offers.
    let error = resolve_rendered(
        &PeppyDirs::new(directory.path()),
        launcher.clone(),
        &[],
        &joins(&["alpha", "alpha"]),
        &JoinPreview::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("`--join real:alpha` names a copy the launch already starts"),
        "{error}"
    );
    let error = resolve_rendered(
        &PeppyDirs::new(directory.path()),
        launcher,
        &[],
        &[core_node_api::encoding::LaunchJoin {
            option: "ghost".into(),
            name: Name::new("alpha").unwrap(),
        }],
        &JoinPreview::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("`ghost` is not an option of a `zero_or_more` axis"),
        "{error}"
    );
}
