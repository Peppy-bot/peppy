//! Launch variables end to end: a launch keeps the value of each variable
//! its launcher names, refuses to start when an instance that it or a join
//! through its scope can start lacks one, and a later join gives the kept
//! value to the run of each instance that names it, and to nothing else.
//! The probe nodes write what their processes received to files outside
//! the data root, so each assertion reads the environment of a real process.

use super::copies::{execute, execute_reporting, participant_request};
use super::robot_leases::InterfaceClient;
use super::*;
use core_node_api::encoding::{
    LaunchWord, PlacementSpec, StackBuildGoal, StackJoinGoal, StackListRequest, StackResetRequest,
};
use daemon_config::daemon_interface::DaemonInterface;
use peppy_codec::consumer::GoalOutcome;
use serde_json::json;

/// The manager, a client of `robot_membership:v1` whose scope offers `arm`.
const MANAGER: &str = "manager_inst";
/// The variable the brain of each arm takes from the launch.
const KEY: &str = "PROBE_KEY";
/// A variable of the launch environment that no instance names.
const OTHER: &str = "OTHER_KEY";
/// The variable the tool of an arm takes, a part only a CLI word selects.
const TOOL_KEY: &str = "TOOL_KEY";

/// The fleet of these tests: its directory, its launcher, and the files the
/// probes append to, one line per process.
struct Fleet {
    _directory: tempfile::TempDir,
    launcher: PathBuf,
    /// `<PROBE_KEY>|<OTHER_KEY>` of each run of a brain, `unset` for a
    /// variable its environment does not hold.
    brain_runs: PathBuf,
    /// `<PROBE_KEY>` of each build of the brain node.
    brain_builds: PathBuf,
    /// `<TOOL_KEY>` of each run of a tool.
    tool_runs: PathBuf,
}

impl Fleet {
    /// Writes the nodes and the launcher of these tests under a new
    /// directory and records the nodes in the nodes cache of `started`: the
    /// axis `control`, whose option `manager` deploys the manager and
    /// `none` nothing, and the axis `robot`, whose option `arm` deploys the
    /// brain `brain_inst`, which takes `PROBE_KEY` from the launch, and whose
    /// own axis `tool` deploys, under `gripper`, the tool `tool_inst`, which
    /// takes `TOOL_KEY`. The scope offers `arm` with no feature.
    fn write(started: &StartedCoreNode) -> Self {
        let directory = tempdir().unwrap();
        let probes = directory.path().join("probes");
        fs::create_dir_all(&probes).unwrap();
        let brain_runs = probes.join("brain_runs");
        let brain_builds = probes.join("brain_builds");
        let tool_runs = probes.join("tool_runs");
        let nodes = directory.path().join("nodes");
        let manager = write_node_config_with_options(
            &nodes,
            "manager",
            "v1",
            NodeConfigOptions {
                run_cmd: &["sleep", "300"],
                depends_on: r#"{ daemon: [{ name: "robot_membership", tag: "v1", link_id: "stack" }] }"#,
                interfaces: r#"{
                    services: { consumes: [{ link_id: "stack", name: "list" }] },
                    actions: { consumes: [
                        { link_id: "stack", name: "join" },
                        { link_id: "stack", name: "remove" },
                    ] },
                }"#,
                ..Default::default()
            },
        );
        let brain_run = format!(
            "printf '%s|%s\\n' \"${{{KEY}-unset}}\" \"${{{OTHER}-unset}}\" >> {}; exec sleep 300",
            brain_runs.display()
        );
        let brain_build = format!(
            "printf '%s\\n' \"${{{KEY}-unset}}\" >> {}",
            brain_builds.display()
        );
        let brain = write_node_config_with_options(
            &nodes,
            "brain_probe",
            "v1",
            NodeConfigOptions {
                build_cmd: &["sh", "-c", &brain_build],
                run_cmd: &["sh", "-c", &brain_run],
                ..Default::default()
            },
        );
        let tool_run = format!(
            "printf '%s\\n' \"${{{TOOL_KEY}-unset}}\" >> {}; exec sleep 300",
            tool_runs.display()
        );
        let tool = write_node_config(&nodes, "tool_probe", "v1", &["sh", "-c", &tool_run]);
        TestPackagesCache::new()
            .fs_entry("manager", "v1", &manager)
            .fs_entry("brain_probe", "v1", &brain)
            .fs_entry("tool_probe", "v1", &tool)
            .write(&started.peppy_dirs);
        let launcher = directory.path().join("fleet.json5");
        fs::write(
            &launcher,
            format!(
                r#"{{
            peppy_schema: "launcher/v1",
            description: ["A test launcher."],
            components: [
                {{ name: "control", cardinality: "one", options: {{
                    manager: {{ deployments: [{{ source: {{ name: "manager", tag: "v1" }}, instances: [
                        {{ instance_id: "{MANAGER}", daemon_scopes: {{ stack: {{
                            max_copies: 4,
                            options: [{{ option: "arm", model: "probe", description: "A probed arm" }}],
                        }} }} }}
                    ] }}] }},
                    none: {{}},
                }} }},
                {{ name: "robot", cardinality: "zero_or_more", options: {{
                    arm: {{
                        deployments: [{{ source: {{ name: "brain_probe", tag: "v1" }}, instances: [
                            {{ instance_id: "brain_inst", env_from_launch: ["{KEY}"] }}
                        ] }}],
                        components: [{{ name: "tool", cardinality: "zero_or_one", options: {{
                            gripper: {{ deployments: [{{ source: {{ name: "tool_probe", tag: "v1" }}, instances: [
                                {{ instance_id: "tool_inst", env_from_launch: ["{TOOL_KEY}"] }}
                            ] }}] }},
                        }} }}],
                    }},
                }} }},
            ],
            deployments: [{{ control: "manager" }}],
        }}"#
            ),
        )
        .unwrap();
        Self {
            _directory: directory,
            launcher,
            brain_runs,
            brain_builds,
            tool_runs,
        }
    }

    /// A launch of the fleet under `launch_id` whose forwarded environment
    /// is `env`.
    fn launch(&self, launch_id: &str, env: &[(&str, &str)]) -> LaunchGoal {
        LaunchGoal::new(
            LauncherOrigin::Fs(self.launcher.clone()),
            launch_id,
            unreached_budgets().with_env_vars(entries(env)),
        )
    }
}

fn entries(env: &[(&str, &str)]) -> Vec<(String, String)> {
    env.iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

/// The lines of the probe file `path`, once it holds `count` of them.
async fn probe_lines(path: &Path, count: usize) -> Vec<String> {
    poll_until(
        RESULT_TIMEOUT,
        &format!("{} should hold {count} lines", path.display()),
        || {
            let lines: Vec<String> = fs::read_to_string(path)
                .ok()?
                .lines()
                .map(str::to_owned)
                .collect();
            (lines.len() >= count).then_some(lines)
        },
    )
    .await
}

/// A join of a copy `name` of `arm` by the manager, through
/// `robot_membership:v1`, which forwards no environment, run to its end.
async fn join_through_the_scope(started: &StartedCoreNode, name: &str) -> GoalOutcome {
    let manager = InterfaceClient::of(started, DaemonInterface::RobotMembership, MANAGER).await;
    let goal = manager
        .send_goal(
            "join",
            json!({
                "name": name,
                "option": "arm",
                "core_node": started.core_node_name,
                "features": [],
            }),
        )
        .await;
    assert!(goal.refusal().is_none(), "{:?}", goal.refusal());
    manager.end_of(&goal).await
}

/// A CLI join of a copy `name` of `arm` with the tool, whose forwarded
/// environment is `env`.
fn cli_join_with_the_tool(name: &str, env: &[(&str, &str)]) -> StackJoinGoal {
    StackJoinGoal {
        selections: vec!["tool=gripper".parse().unwrap()],
        ..StackJoinGoal::new(
            format!("arm:{name}").parse().unwrap(),
            unreached_budgets().with_env_vars(entries(env)),
        )
    }
}

/// The names of the copies on the stack.
async fn copies_on_the_stack(started: &StartedCoreNode) -> Vec<String> {
    participant_request(started, &StackListRequest::new())
        .await
        .copies
        .iter()
        .map(|copy| copy.name.to_string())
        .collect()
}

/// Every file under `root` whose bytes hold `value`.
fn files_holding(root: &Path, value: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file()
                && fs::read(&path).is_ok_and(|bytes| {
                    bytes
                        .windows(value.len())
                        .any(|window| window == value.as_bytes())
                })
            {
                found.push(path);
            }
        }
    }
    found
}

/// A launch with `--no-prebuild` and the key, then a join through the
/// scope: the brain's run reads the launch's value and no variable the
/// brain does not name, its build reads none, and the value is in no
/// feedback, no listing, no answer of the scope and no file of the data
/// root.
#[tokio::test]
async fn a_join_through_the_scope_gives_the_run_of_the_brain_the_kept_value() {
    const SENTINEL: &str = "sentinel-7f3e9a";
    let started = start_core_node_with_mock_messenger().await;
    let fleet = Fleet::write(&started);
    let mut responders = answer_readiness(&started, "manager", MANAGER).await;
    responders.extend(answer_readiness(&started, "brain_probe", "alpha_brain_inst").await);

    let (launched, feedback) = execute_reporting(
        &started,
        &fleet
            .launch("kept-value", &[(KEY, SENTINEL), (OTHER, "other")])
            .with_prebuild(false),
    )
    .await;
    launched.assert_completed();
    assert_eq!(
        join_through_the_scope(&started, "alpha").await,
        GoalOutcome::Completed(json!({}))
    );

    assert_eq!(
        probe_lines(&fleet.brain_runs, 1).await,
        [format!("{SENTINEL}|unset")],
        "the run takes the kept value of the variable it names, and no other"
    );
    assert_eq!(
        probe_lines(&fleet.brain_builds, 1).await,
        ["unset"],
        "the build of a join through the scope gets no environment"
    );

    assert!(
        feedback
            .iter()
            .all(|output| !output.line.contains(SENTINEL)),
        "{feedback:?}"
    );
    let listed = participant_request(&started, &StackListRequest::new()).await;
    assert!(!format!("{listed:?}").contains(SENTINEL), "{listed:?}");
    let manager = InterfaceClient::of(&started, DaemonInterface::RobotMembership, MANAGER).await;
    let answer = manager.call("list", json!({})).await.expect("the list");
    assert!(!answer.to_string().contains(SENTINEL), "{answer}");
    assert_eq!(
        files_holding(started.peppy_dirs.root(), SENTINEL),
        Vec::<PathBuf>::new(),
        "no file of the data root holds the value"
    );
    assert_eq!(
        files_holding(fleet.brain_runs.parent().unwrap(), SENTINEL),
        std::slice::from_ref(&fleet.brain_runs),
        "the scan finds the value where the brain wrote it"
    );
}

/// A launch without the key is refused before it builds anything, naming
/// the variable, the brain, its option and the manager whose scope offers
/// it; the running stack stays, and a join after the refusal reads the
/// value that the running stack keeps.
#[tokio::test]
async fn a_launch_without_the_key_is_refused_and_the_running_stack_keeps_its_value() {
    let started = start_core_node_with_mock_messenger().await;
    let fleet = Fleet::write(&started);
    let mut responders = answer_readiness(&started, "manager", MANAGER).await;
    responders.extend(answer_readiness(&started, "brain_probe", "alpha_brain_inst").await);
    responders.extend(answer_readiness(&started, "brain_probe", "bravo_brain_inst").await);

    execute(&started, &fleet.launch("first", &[(KEY, "first-value")]))
        .await
        .assert_completed();
    assert_eq!(
        join_through_the_scope(&started, "alpha").await,
        GoalOutcome::Completed(json!({}))
    );
    let builds = probe_lines(&fleet.brain_builds, 1).await;

    let refused = execute(&started, &fleet.launch("second", &[(OTHER, "other")])).await;
    refused.assert_failed(
        PeppyFailureKind::Rejected,
        "the launch needs `PROBE_KEY`, and the launch environment does not hold it, or holds a \
         value with characters that a node cannot receive: `brain_inst` of option `arm` takes it \
         from the launch, and stack.join through `manager_inst` can add a copy of `arm`. Export \
         `PROBE_KEY` in the shell that launches",
    );
    assert_eq!(
        fs::read_to_string(&fleet.brain_builds)
            .unwrap()
            .lines()
            .count(),
        builds.len(),
        "the refused launch builds nothing"
    );
    assert_eq!(copies_on_the_stack(&started).await, ["alpha"]);

    assert_eq!(
        join_through_the_scope(&started, "bravo").await,
        GoalOutcome::Completed(json!({}))
    );
    assert_eq!(
        probe_lines(&fleet.brain_runs, 2).await,
        ["first-value|unset", "first-value|unset"]
    );
}

/// With `control=none` no scope offers a copy, so a launch without the key
/// starts, and so does a build, which starts nothing.
#[tokio::test]
async fn a_launch_that_no_join_can_add_a_brain_to_and_a_build_need_no_key() {
    let started = start_core_node_with_mock_messenger().await;
    let fleet = Fleet::write(&started);
    let _responders = answer_readiness(&started, "manager", MANAGER).await;

    let without_scope = fleet
        .launch("no-scope", &[])
        .with_layout(
            PlacementSpec::default(),
            vec!["control=none".parse::<LaunchWord>().unwrap()],
            Vec::new(),
        )
        .unwrap();
    execute(&started, &without_scope).await.assert_completed();

    let build = StackBuildGoal::from(fleet.launch("build", &[]));
    execute(&started, &build).await.assert_completed();
}

/// A tool that only a CLI word selects makes no launch need its key. A CLI
/// join that does not forward the key is refused while the launch kept no
/// value, and one that forwards it starts the tool with it. After a launch
/// with the key, a CLI join that forwards none starts the tool with the
/// kept value, and one that forwards another value starts it with that one.
#[tokio::test]
async fn a_cli_join_reads_its_own_value_then_the_one_the_launch_kept() {
    let started = start_core_node_with_mock_messenger().await;
    let fleet = Fleet::write(&started);
    let mut responders = answer_readiness(&started, "manager", MANAGER).await;
    for copy in ["alpha", "bravo", "charlie", "delta"] {
        responders
            .extend(answer_readiness(&started, "brain_probe", &format!("{copy}_brain_inst")).await);
        responders
            .extend(answer_readiness(&started, "tool_probe", &format!("{copy}_tool_inst")).await);
    }

    execute(
        &started,
        &fleet.launch("no-tool-key", &[(KEY, "brain-value")]),
    )
    .await
    .assert_completed();
    execute(
        &started,
        &cli_join_with_the_tool("alpha", &[(KEY, "brain-value")]),
    )
    .await
    .assert_failed(
        PeppyFailureKind::Rejected,
        "the join needs `TOOL_KEY`, and neither the join environment nor the launch environment \
         holds it, or they hold a value with characters that a node cannot receive: \
         `alpha_tool_inst` takes it from the launch, and the join starts it",
    );
    execute(
        &started,
        &cli_join_with_the_tool("bravo", &[(TOOL_KEY, "joined-value")]),
    )
    .await
    .assert_completed();
    assert_eq!(probe_lines(&fleet.tool_runs, 1).await, ["joined-value"]);
    assert_eq!(
        probe_lines(&fleet.brain_runs, 1).await,
        ["brain-value|unset"],
        "the brain of the CLI join takes the kept value"
    );

    execute(
        &started,
        &fleet.launch(
            "tool-key",
            &[(KEY, "brain-value"), (TOOL_KEY, "kept-value")],
        ),
    )
    .await
    .assert_completed();
    execute(&started, &cli_join_with_the_tool("charlie", &[]))
        .await
        .assert_completed();
    execute(
        &started,
        &cli_join_with_the_tool("delta", &[(TOOL_KEY, "newer-value")]),
    )
    .await
    .assert_completed();
    assert_eq!(
        probe_lines(&fleet.tool_runs, 3).await,
        ["joined-value", "kept-value", "newer-value"]
    );
}

/// A stack reset drops the kept values, and a new launch keeps its own: a
/// join after them reads the new value.
#[tokio::test]
async fn a_new_launch_after_a_reset_gives_its_own_value() {
    let started = start_core_node_with_mock_messenger().await;
    let fleet = Fleet::write(&started);
    let mut responders = answer_readiness(&started, "manager", MANAGER).await;
    responders.extend(answer_readiness(&started, "brain_probe", "alpha_brain_inst").await);

    execute(&started, &fleet.launch("old", &[(KEY, "old-value")]))
        .await
        .assert_completed();
    participant_request(&started, &StackResetRequest::new()).await;
    execute(&started, &fleet.launch("new", &[(KEY, "new-value")]))
        .await
        .assert_completed();
    assert_eq!(
        join_through_the_scope(&started, "alpha").await,
        GoalOutcome::Completed(json!({}))
    );
    assert_eq!(probe_lines(&fleet.brain_runs, 1).await, ["new-value|unset"]);
}
