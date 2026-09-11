use clap::Parser;
use config::runtime::{CoreNodeName, Name};
use core_node_api::encoding::JoinPlacement;
use daemon_config::consts::PeppyDirs;
use peppy::commands::stack::{JoinPreview, StackCommands, resolve_rendered};

#[derive(Parser)]
struct StackCli {
    #[command(subcommand)]
    command: StackCommands,
}

#[test]
fn stack_join_cli_names_the_option_and_accepts_placement_overrides_and_timeouts() {
    let cli = StackCli::try_parse_from([
        "stack",
        "join",
        "real",
        "-i",
        "alpha",
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
        option,
        name,
        with,
        arguments,
        place,
        timeouts,
        ..
    } = cli.command
    else {
        panic!("join")
    };
    assert_eq!(option, "real");
    assert_eq!(name.as_str(), "alpha");
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
        vec!["stack", "join", "-i", "alpha"],
        vec!["stack", "join", "real", "-i", "bad/name"],
        vec!["stack", "join", "real", "-i", "self"],
        vec!["stack", "resolve", "fleet", "--join-with", "xr"],
        vec!["stack", "resolve", "fleet", "--join-name", "alpha"],
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
    let error = StackCli::try_parse_from([
        "stack",
        "join",
        "real",
        "-i",
        "alpha",
        "--place",
        "alpha@jetson-1",
    ])
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
            vec!["stack", "join", "real", "-i", "alpha"],
            vec!["stack", "resolve", "fleet", "--join", "real"],
        ] {
            let flag = if prefix.contains(&"resolve") {
                "--join-set-arguments"
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
        &JoinPreview::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("peppy stack join real -i NAME"), "{error}");
}

#[test]
fn resolve_previews_a_join_with_its_own_name_selection_and_overrides() {
    let cli = StackCli::try_parse_from([
        "stack",
        "resolve",
        "openarm_fleet",
        "--with",
        "mujoco",
        "--join",
        "openarm_v2_sim",
        "--join-name",
        "bravo",
        "--join-with",
        "xr_commander",
        "--join-set-arguments",
        "commander_inst.port=8001",
    ])
    .unwrap();
    let StackCommands::Resolve { with, join, .. } = cli.command else {
        unreachable!()
    };
    assert_eq!(with.words, ["mujoco"]);
    assert_eq!(join.option.as_deref(), Some("openarm_v2_sim"));
    assert_eq!(join.name.as_str(), "bravo");
    assert_eq!(join.words, ["xr_commander"]);
    assert_eq!(
        join.arguments,
        ["commander_inst.port=8001".parse().unwrap()]
    );
    let cli = StackCli::try_parse_from([
        "stack",
        "resolve",
        "openarm_fleet",
        "--join",
        "openarm_v2_sim",
    ])
    .unwrap();
    let StackCommands::Resolve { join, .. } = cli.command else {
        unreachable!()
    };
    assert_eq!(join.name.as_str(), "preview");
}
