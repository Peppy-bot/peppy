//! The restart policy as the CLI and a launch resolve, show and apply it:
//! the three levels, the flags, the listing, the info line, the resolve
//! report, the stack changes made while an instance waits, and the pairs
//! and observations an instance comes back with.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use config::failure_kind::PeppyFailureKind;
use config::node::{Cardinality, RestartOrigin, RestartPolicy};
use config::runtime::{BoundProducers, CopyName, Name};
use core_node::RestartBackoff;
use core_node::test_support::{
    EXIT_CLEANLY_ON_TERM, dump_runtime_config, framework_entry, instant_restart_backoff,
    parked_restart_backoff, poll_file, signal_process,
};
use core_node_api::{ExitCause, InstanceState, SerializedInstance, SerializedNodeGraph};
use daemon_config::consts::PeppyDirs;
use node_stack::action_log::test_support::nth_newest_run_log;
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands};
use peppy::commands::stack::{LaunchWords, StackCommand, StackCommands};
use peppy::context::AppContext;
use peppy::test_support::{InstanceLifetime, ServeCommandEmulation, default_arguments};
use peppylib::MessengerHandle;
use peppylib::messaging::{BoundSetState, ObservationState};
use peppylib::services::binding_update::listen_for_binding_update;
use peppylib::services::observation_update::listen_for_observation_update;
use peppylib::services::slot_update::SlotChannel;
use tokio::sync::watch;

use super::common::{
    add_built_node, assert_daemon_failure, emulate_cooperative_shutdown, emulate_pairing_instance,
    emulate_startup_services, install_node_manifest, launcher_args, node_add_command,
    node_run_command, pairing_node_config, pairing_node_config_writing_pid_to,
    read_daemon_git_hash, register_repo_caches, seed_pairing_repo, stack_graph, test_node_target,
    timeouts, write_node_config_for_helper,
};

const TAG: &str = "v1";
const WAIT: Duration = Duration::from_secs(15);
/// How long a test watches for something that must not happen: past the
/// requests a restarted process makes once it is `running`.
const QUIET: Duration = Duration::from_millis(1500);
/// The bound on a whole stack command here, as the launch tests set it.
const MAX_STACK_COMMAND_SECS: Option<u64> = Some(120);

/// A daemon with an injected restart backoff, with a work directory the
/// commands run in.
struct Harness {
    serve: ServeCommandEmulation,
    ctx: Arc<AppContext>,
    messenger: MessengerHandle,
    core_node_name: String,
    _work_dir: tempfile::TempDir,
}

impl Harness {
    /// Mocks the startup services of `instance_id` of `node`.
    async fn emulate_startup(&self, node: &str, instance_id: &str) {
        emulate_startup_services(&self.messenger, &self.core_node_name, node, instance_id).await;
    }
}

/// A daemon whose restart backoff has no wait.
async fn harness() -> Harness {
    harness_with(instant_restart_backoff()).await
}

async fn harness_with(restart_backoff: RestartBackoff) -> Harness {
    let serve = ServeCommandEmulation::with_mock_and_arguments(core_node::CoreNodeArguments {
        restart_backoff,
        ..default_arguments()
    })
    .await
    .expect("serve emulation");
    let core_node_name = serve.core_node_name().to_string();
    let messenger = MessengerHandle::from_shared(serve.messenger());
    let work_dir = tempfile::tempdir().expect("work dir");
    let ctx = super::common::serve_app_context(&serve, work_dir.path());
    Harness {
        serve,
        ctx,
        messenger,
        core_node_name,
        _work_dir: work_dir,
    }
}

/// A node whose process records its pid to `pidfile` and runs until killed.
fn pidfile_node(name: &str, pidfile: &Path, framework: Option<&str>) -> String {
    pidfile_node_running(name, pidfile, framework, "exec sleep 300")
}

/// A node whose process records its pid to `pidfile` and exits 0 on SIGTERM.
fn trap_exit_node(name: &str, pidfile: &Path, framework: Option<&str>) -> String {
    pidfile_node_running(name, pidfile, framework, EXIT_CLEANLY_ON_TERM)
}

/// A node whose process records its pid to `pidfile` and then runs `body`.
fn pidfile_node_running(name: &str, pidfile: &Path, framework: Option<&str>, body: &str) -> String {
    node_writing_pid_to(name, &format!("'{}'", pidfile.display()), framework, body)
}

/// A node whose process records its pid under its instance id in `pid_dir`
/// and runs until killed, so instances of one node have one file each.
fn per_instance_pidfile_node(name: &str, pid_dir: &Path, framework: Option<&str>) -> String {
    node_writing_pid_to(
        name,
        &per_instance_pidfile_word(pid_dir),
        framework,
        "exec sleep 300",
    )
}

/// The shell word naming `<pid_dir>/<instance id>.pid`: the working
/// directory's name is the instance id.
fn per_instance_pidfile_word(pid_dir: &Path) -> String {
    format!("'{}/'$(basename \\\"$PWD\\\")'.pid'", pid_dir.display())
}

/// A node whose process records its pid to the shell word `pidfile_word`
/// and then runs `body`.
fn node_writing_pid_to(
    name: &str,
    pidfile_word: &str,
    framework: Option<&str>,
    body: &str,
) -> String {
    let framework = framework_entry(framework);
    format!(
        r#"{{
            peppy_schema: "node/v1",
            manifest: {{ name: "{name}", tag: "{TAG}" }},
            execution: {{
                language: "rust",
                run_cmd: ["sh", "-c", "echo $$ > {pidfile_word}; {body}"],
            }},
            {framework}
        }}"#
    )
}

fn read_pid(pidfile: &Path) -> u32 {
    try_read_pid(pidfile).expect("the pidfile holds a pid")
}

/// The pid in `pidfile`, once the process wrote it whole.
fn try_read_pid(pidfile: &Path) -> Option<u32> {
    std::fs::read_to_string(pidfile).ok()?.trim().parse().ok()
}

/// The pid the next process recorded in `pidfile` over `previous`: a
/// restarted process is `running` once its probes answer, which the mocks
/// do before its shell writes the file.
async fn wait_for_new_pid(h: &Harness, pidfile: &Path, previous: u32) -> u32 {
    let mut recorded = None;
    wait_until(h, "the next process recorded its pid", async || {
        recorded = try_read_pid(pidfile).filter(|pid| *pid != previous);
        recorded.is_some()
    })
    .await;
    recorded.expect("a new pid")
}

fn kill(pid: u32) {
    signal_process(pid, "-9");
}

fn terminate(pid: u32) {
    signal_process(pid, "-TERM");
}

async fn graph(h: &Harness) -> SerializedNodeGraph {
    let handle = h.ctx.messenger_handle().expect("messenger handle");
    stack_graph(handle, &h.core_node_name).await
}

async fn instance(h: &Harness, instance_id: &str) -> SerializedInstance {
    graph(h)
        .await
        .nodes
        .into_iter()
        .flat_map(|node| node.instances)
        .find(|instance| instance.instance_id == instance_id)
        .unwrap_or_else(|| panic!("`{instance_id}` is in the stack"))
}

fn peppy_dirs(h: &Harness) -> PeppyDirs {
    PeppyDirs::new(h.serve.temp_dir())
}

fn stack_log(h: &Harness) -> String {
    std::fs::read_to_string(peppy_dirs(h).stack_log_path()).unwrap_or_default()
}

/// The run log of `instance_id`'s current process, the newest of its files;
/// empty before its first process.
fn run_log(h: &Harness, instance_id: &str) -> String {
    nth_newest_run_log(&peppy_dirs(h).logs_dir_run(), instance_id, 0)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default()
}

/// The run log of `instance_id`'s restarted process, which opens with the
/// line naming its attempt.
fn restarted_run_log(h: &Harness, instance_id: &str) -> String {
    let log = run_log(h, instance_id);
    assert!(
        log.lines()
            .next()
            .is_some_and(|line| line.contains("restart attempt")),
        "`{instance_id}`'s newest run log is a restart's:\n{log}"
    );
    log
}

async fn wait_until(h: &Harness, what: &str, holds: impl AsyncFnMut() -> bool) {
    wait_until_within(h, WAIT, what, holds).await;
}

async fn wait_until_within(
    h: &Harness,
    limit: Duration,
    what: &str,
    mut holds: impl AsyncFnMut() -> bool,
) {
    let deadline = std::time::Instant::now() + limit;
    while !holds().await {
        assert!(
            std::time::Instant::now() < deadline,
            "{what}; stack log:\n{}",
            stack_log(h)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_restarts(h: &Harness, instance_id: &str, restarts: u32) {
    wait_until(
        h,
        &format!("`{instance_id}` runs again with {restarts} restarts"),
        async || {
            let instance = instance(h, instance_id).await;
            instance.state == InstanceState::Running
                && instance.restart_status.restart_count == restarts
        },
    )
    .await;
}

async fn wait_for_state(h: &Harness, instance_id: &str, state: InstanceState) {
    wait_until(h, &format!("`{instance_id}` is {state}"), async || {
        instance(h, instance_id).await.state == state
    })
    .await;
}

fn launch(h: &Harness, launcher: &Path) -> Result<(), peppy::error::Error> {
    launch_with(h, launcher, &[])
}

/// `stack launch` of `launcher` with the options `with` selects.
fn launch_with(h: &Harness, launcher: &Path, with: &[&str]) -> Result<(), peppy::error::Error> {
    let mut args = launcher_args(launcher.to_path_buf(), MAX_STACK_COMMAND_SECS);
    args.with = LaunchWords {
        words: with
            .iter()
            .map(|word| word.parse().expect("a launch word"))
            .collect(),
    };
    StackCommand {
        command: StackCommands::Launch(args),
    }
    .execute(&h.ctx)
}

fn join(h: &Harness, option: &str, name: &str) -> Result<(), peppy::error::Error> {
    StackCommand {
        command: StackCommands::Join {
            copy: format!("{option}:{name}")
                .parse()
                .expect("a copy reference"),
            with: Default::default(),
            arguments: Vec::new(),
            timeouts: timeouts(MAX_STACK_COMMAND_SECS),
        },
    }
    .execute(&h.ctx)
}

/// Removes the copy `name` from the stack, trying again while a start holds
/// the daemon: a restart's start holds it until its run returns, a moment
/// after the listing shows its instance running.
async fn remove_copy(h: &Harness, name: &str) {
    wait_until(h, &format!("stack remove {name}"), async || {
        let removal = StackCommand {
            command: StackCommands::Remove {
                name: CopyName::new(name).expect("a copy name"),
            },
        }
        .execute(&h.ctx);
        match removal {
            Ok(()) => true,
            Err(e) if e.to_string().contains("in progress on this daemon") => false,
            Err(e) => panic!("stack remove {name}: {e}"),
        }
    })
    .await;
}

fn stop(h: &Harness, instance_id: &str) {
    NodeCommand {
        command: NodeCommands::Stop {
            instance_id: instance_id.to_owned(),
        },
    }
    .execute(&h.ctx)
    .unwrap_or_else(|e| panic!("node stop {instance_id}: {e}"));
}

/// Installs the manifest of `node` under `nodes_dir` and registers it in the
/// daemon's node cache, as a launch reads it.
fn install_cached_node(h: &Harness, nodes_dir: &Path, node: &str, manifest: &str) -> PathBuf {
    let git_hash = read_daemon_git_hash(h.serve.daemon_state_path());
    let path = install_node_manifest(nodes_dir, node, &git_hash, manifest);
    register_repo_caches(h.serve.temp_dir(), &[(node, TAG, path.as_path())]);
    path
}

/// `peppy node run` of `node` as `instance_id` with `--restart` and
/// `--max-restarts` as given.
fn run_with_restart(
    instance_id: &str,
    node: &str,
    restart_flag: Option<RestartPolicy>,
    max_restarts_flag: Option<std::num::NonZeroU32>,
) -> NodeCommand {
    let mut run = node_run_command(instance_id, node, Vec::new(), Vec::new());
    if let NodeCommands::Run {
        restart,
        max_restarts,
        ..
    } = &mut run.command
    {
        *restart = restart_flag;
        *max_restarts = max_restarts_flag;
    }
    run
}

/// `peppy node run --restart on_failure` of a node with no policy restarts
/// it after a crash, the flag wins over the manifest, and the manifest's
/// policy applies without a flag.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_run_restart_flag_and_manifest_policy() {
    let h = harness().await;
    let pid_dir = tempfile::tempdir().expect("pid dir");
    let plain_pidfile = pid_dir.path().join("plain.pid");
    let plain_dir = tempfile::tempdir().expect("node dir");
    add_built_node(
        &h.ctx,
        plain_dir.path(),
        &pidfile_node("plain", &plain_pidfile, None),
    );
    let declared_dir = tempfile::tempdir().expect("node dir");
    add_built_node(
        &h.ctx,
        declared_dir.path(),
        &per_instance_pidfile_node(
            "declared",
            pid_dir.path(),
            Some(r#"{ restart: "on_failure" }"#),
        ),
    );

    h.emulate_startup("plain", "plain_inst").await;
    run_with_restart("plain_inst", "plain", Some(RestartPolicy::OnFailure), None)
        .execute(&h.ctx)
        .expect("node run --restart on_failure");
    let listed = instance(&h, "plain_inst").await;
    assert_eq!(
        listed.restart_status.restart.policy(),
        RestartPolicy::OnFailure
    );
    assert_eq!(listed.restart_status.restart_origin, RestartOrigin::Flag);
    kill(read_pid(&plain_pidfile));
    wait_for_restarts(&h, "plain_inst", 1).await;
    let listing = peppy::commands::stack::list_nodes_collecting(&h.ctx, false, None)
        .await
        .expect("stack list")
        .output;
    assert!(
        listing.contains("RESTARTS"),
        "the listing has a RESTARTS column:\n{listing}"
    );
    fn cells(line: &str) -> Vec<&str> {
        line.split('│').map(str::trim).collect()
    }
    let restarts_column = listing
        .lines()
        .find_map(|line| cells(line).iter().position(|cell| *cell == "RESTARTS"))
        .expect("the RESTARTS header");
    let row = listing
        .lines()
        .find(|line| line.contains("plain_inst"))
        .expect("the instance's row");
    assert!(row.contains("running"), "{row}");
    assert_eq!(cells(row)[restarts_column], "1", "{row}");
    assert!(
        stack_log(&h).contains("Instance 'plain_inst' of node 'plain:v1' restarted (attempt 1)"),
        "{}",
        stack_log(&h)
    );

    h.emulate_startup("declared", "declared_inst").await;
    node_run_command("declared_inst", "declared", Vec::new(), Vec::new())
        .execute(&h.ctx)
        .expect("node run");
    let listed = instance(&h, "declared_inst").await;
    assert_eq!(
        listed.restart_status.restart.policy(),
        RestartPolicy::OnFailure
    );
    assert_eq!(listed.restart_status.restart_origin, RestartOrigin::Node);
    kill(read_pid(&pid_dir.path().join("declared_inst.pid")));
    wait_for_restarts(&h, "declared_inst", 1).await;
    assert!(
        stack_log(&h)
            .contains("Instance 'declared_inst' of node 'declared:v1' restarted (attempt 1)"),
        "{}",
        stack_log(&h)
    );

    // The flag wins over the manifest.
    h.emulate_startup("declared", "declared_never").await;
    run_with_restart(
        "declared_never",
        "declared",
        Some(RestartPolicy::Never),
        None,
    )
    .execute(&h.ctx)
    .expect("node run --restart never");
    kill(read_pid(&pid_dir.path().join("declared_never.pid")));
    wait_until(&h, "`declared_never` fails", async || {
        instance(&h, "declared_never").await.state == InstanceState::Failed
    })
    .await;

    // `peppy node run` under a waiting or tracked id is refused by the
    // preflight naming the state, before the daemon opens a run log for it.
    let run_log_before = run_log(&h, "declared_never");
    let err = node_run_command("declared_never", "declared", Vec::new(), Vec::new())
        .execute(&h.ctx)
        .expect_err("a tracked id is taken");
    assert_eq!(
        err.to_string(),
        format!(
            "Instance ID `declared_never` already exists for node `declared:v1` and is failed; \
             `peppy node stop declared_never --core-node {}` frees the id",
            h.core_node_name
        )
    );
    assert_eq!(
        run_log(&h, "declared_never"),
        run_log_before,
        "the daemon was not asked"
    );
}

/// `--max-restarts` caps the attempts in a row, `node info` prints the
/// declaration with its origin and the attempt, and a cap on `never` is
/// refused before the daemon is asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_run_max_restarts_flag() {
    let h = harness().await;
    let pid_dir = tempfile::tempdir().expect("pid dir");
    let pidfile = pid_dir.path().join("capped.pid");
    let dir = tempfile::tempdir().expect("node dir");
    add_built_node(&h.ctx, dir.path(), &pidfile_node("capped", &pidfile, None));
    h.emulate_startup("capped", "capped_inst").await;

    let err = run_with_restart(
        "refused",
        "capped",
        Some(RestartPolicy::Never),
        std::num::NonZeroU32::new(2),
    )
    .execute(&h.ctx)
    .expect_err("never takes no cap");
    assert_eq!(
        err.to_string(),
        "`--max-restarts` needs `--restart on_failure` or `--restart always`"
    );
    // `node add --run` refuses the same flags before it adds anything.
    let fresh_dir = tempfile::tempdir().expect("node dir");
    std::fs::write(
        fresh_dir.path().join("peppy.json5"),
        pidfile_node("never_added", &pid_dir.path().join("never_added.pid"), None),
    )
    .expect("write node config");
    let mut add_and_run = node_add_command(fresh_dir.path());
    if let NodeCommands::Add {
        run,
        restart,
        max_restarts,
        ..
    } = &mut add_and_run.command
    {
        *run = true;
        *restart = Some(RestartPolicy::Never);
        *max_restarts = std::num::NonZeroU32::new(2);
    }
    let err = add_and_run.execute(&h.ctx).expect_err("never takes no cap");
    assert_eq!(
        err.to_string(),
        "`--max-restarts` needs `--restart on_failure` or `--restart always`"
    );
    assert!(
        !graph(&h)
            .await
            .nodes
            .iter()
            .any(|node| node.name == "never_added"),
        "nothing was added"
    );
    let mut publisher = run_with_restart(
        "pub_inst",
        "capped",
        Some(RestartPolicy::OnFailure),
        std::num::NonZeroU32::new(2),
    );
    if let NodeCommands::Run { publish_clock, .. } = &mut publisher.command {
        *publish_clock = Some(Name::new("sim").expect("a domain name"));
    }
    h.emulate_startup("capped", "pub_inst").await;
    publisher
        .execute(&h.ctx)
        .expect("a clock publisher takes a policy like any instance");
    let info = node_info(&h, "capped").await;
    assert!(
        info.contains(
            "- pub_inst  [running]  healthy  restart on_failure, max_restarts 2 (--restart), 0 restarts"
        ),
        "{info}"
    );

    run_with_restart(
        "capped_inst",
        "capped",
        Some(RestartPolicy::OnFailure),
        std::num::NonZeroU32::new(2),
    )
    .execute(&h.ctx)
    .expect("node run --restart on_failure --max-restarts 2");
    let info = node_info(&h, "capped").await;
    assert!(
        info.contains(
            "- capped_inst  [running]  healthy  restart on_failure, max_restarts 2 (--restart), 0 restarts"
        ),
        "{info}"
    );
    let mut pid = read_pid(&pidfile);
    for restarts in 1..=2 {
        kill(pid);
        wait_for_restarts(&h, "capped_inst", restarts).await;
        pid = wait_for_new_pid(&h, &pidfile, pid).await;
    }
    kill(pid);
    wait_until(&h, "`capped_inst` stays failed", async || {
        instance(&h, "capped_inst").await.state == InstanceState::Failed
    })
    .await;
    let listed = instance(&h, "capped_inst").await;
    assert_eq!(
        listed.restart_status.restart.max_restarts(),
        std::num::NonZeroU32::new(2)
    );
    assert_eq!(listed.restart_status.attempt, 3);
    assert_eq!(listed.restart_status.restart_count, 2);
    assert_eq!(listed.restart_status.next_start_ms, None);
    assert_eq!(
        listed.restart_status.last_exit,
        Some(ExitCause::signal(9, false))
    );
    let info = node_info(&h, "capped").await;
    assert!(
        info.contains(
            "- capped_inst  [failed]  -  restart on_failure, max_restarts 2 (--restart), 2 restarts, \
             attempt 3 refused: max_restarts 2 reached, failed starts included, last exit: signal 9"
        ),
        "{info}"
    );
    assert!(
        stack_log(&h).contains("`max_restarts` reached: 2 attempts in a row"),
        "{}",
        stack_log(&h)
    );
}

async fn node_info(h: &Harness, node: &str) -> String {
    peppy::commands::node::node_info_rendered(&h.ctx, node, TAG)
        .await
        .expect("node info")
}

/// A launch resolves the three levels: the launcher root for an instance
/// with no other say, the entry and an adjustment over the root, and the
/// node's manifest over the root, for a clock publisher as for any instance.
/// `stack resolve` prints each with its origin and resolves what needs no
/// manifest when one is not cached; the instances the root, the entry, the
/// adjustment and a publisher's manifest cover restart after a crash, and
/// the manifest's `never` leaves a clean exit finished.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_levels_resolve_as_documented() {
    let h = harness().await;
    let nodes_dir = tempfile::tempdir().expect("nodes dir");
    let git_hash = read_daemon_git_hash(h.serve.daemon_state_path());
    let pid_dir = tempfile::tempdir().expect("pid dir");
    let mut paths: Vec<(&str, PathBuf)> = Vec::new();
    for (node, framework, clean_exit) in [
        ("by_root", None, false),
        ("by_entry", Some(r#"{ restart: "never" }"#), false),
        ("by_adjustment", Some(r#"{ restart: "never" }"#), false),
        ("by_manifest", Some(r#"{ restart: "never" }"#), true),
        (
            "by_publisher",
            Some(r#"{ restart: { policy: "on_failure", max_restarts: 2 } }"#),
            false,
        ),
        ("root_publisher", None, false),
    ] {
        let pidfile = pid_dir.path().join(format!("{node}.pid"));
        let manifest = if clean_exit {
            trap_exit_node(node, &pidfile, framework)
        } else {
            pidfile_node(node, &pidfile, framework)
        };
        let path = install_node_manifest(nodes_dir.path(), node, &git_hash, &manifest);
        paths.push((node, path));
    }
    let registered: Vec<(&str, &str, &Path)> = paths
        .iter()
        .map(|(node, path)| (*node, TAG, path.as_path()))
        .collect();
    register_repo_caches(h.serve.temp_dir(), &registered);
    let write_launcher = |name: &str, extra_clocks: &str, extra_deployment: &str| {
        let launcher = nodes_dir.path().join(name);
        std::fs::write(
            &launcher,
            format!(
                r#"{{
                    peppy_schema: "launcher/v1",
                    framework: {{ restart: "always", clocks: {{
                        sim: {{ publisher: "pub_inst" }},
                        sim2: {{ publisher: "pub2_inst" }},
                        {extra_clocks}
                    }} }},
                    components: [{{ name: "mode", cardinality: "zero_or_one", options: {{ field: {{
                        adjustments: [{{ target: "adjusted_inst", set_framework: {{ restart: "on_failure" }} }}],
                    }} }} }}],
                    deployments: [
                        {{ source: {{ name: "by_root", tag: "v1" }}, instances: [{{ instance_id: "root_inst" }}] }},
                        {{ source: {{ name: "by_entry", tag: "v1" }},
                          instances: [{{ instance_id: "entry_inst", framework: {{ restart: {{ policy: "on_failure", max_restarts: 5 }} }} }}] }},
                        {{ source: {{ name: "by_adjustment", tag: "v1" }}, instances: [{{ instance_id: "adjusted_inst" }}] }},
                        {{ source: {{ name: "by_manifest", tag: "v1" }}, instances: [{{ instance_id: "manifest_inst" }}] }},
                        {{ source: {{ name: "by_publisher", tag: "v1" }}, instances: [{{ instance_id: "pub_inst" }}] }},
                        {{ source: {{ name: "root_publisher", tag: "v1" }}, instances: [{{ instance_id: "pub2_inst" }}] }},
                        {extra_deployment}
                    ],
                }}"#
            ),
        )
        .expect("launcher");
        launcher
    };
    let launcher = write_launcher("peppy_launcher.json5", "", "");
    let resolve = |launcher: PathBuf| {
        peppy::commands::stack::resolve_rendered(
            &peppy_dirs(&h),
            launcher,
            &["field".parse().expect("a launch word")],
            &[],
        )
    };

    let (_, report) = resolve(launcher.clone()).expect("stack resolve");
    for line in [
        "root_inst: clock `wall`, restart always (launcher root)",
        "entry_inst: clock `wall`, restart on_failure, max_restarts 5 (instance entry)",
        "adjusted_inst: clock `wall`, restart on_failure (instance entry)",
        "manifest_inst: clock `wall`, restart never (peppy.json5)",
        "pub_inst: publishes clock `sim`, restart on_failure, max_restarts 2 (peppy.json5)",
        "pub2_inst: publishes clock `sim2`, restart always (launcher root)",
    ] {
        assert!(report.iter().any(|l| l == line), "`{line}` in {report:?}");
    }
    let unresolved = write_launcher(
        "unresolved.json5",
        r#"sim4: { publisher: "nobody_pub" },"#,
        r#"{ source: { name: "by_nobody", tag: "v1" }, instances: [
              { instance_id: "nobody_inst" },
              { instance_id: "nobody_entry", framework: { restart: "on_failure" } },
              { instance_id: "nobody_pub" },
           ] },"#,
    );
    let (_, report) = resolve(unresolved).expect("stack resolve");
    for line in [
        "nobody_inst: clock `wall`, restart not resolved (manifest unavailable)",
        "nobody_entry: clock `wall`, restart on_failure (instance entry)",
        "nobody_pub: publishes clock `sim4`, restart not resolved (manifest unavailable)",
    ] {
        assert!(report.iter().any(|l| l == line), "`{line}` in {report:?}");
    }

    for (node, id) in [
        ("by_root", "root_inst"),
        ("by_entry", "entry_inst"),
        ("by_adjustment", "adjusted_inst"),
        ("by_manifest", "manifest_inst"),
        ("by_publisher", "pub_inst"),
        ("root_publisher", "pub2_inst"),
    ] {
        h.emulate_startup(node, id).await;
    }
    launch_with(&h, &launcher, &["field"]).expect("stack launch");

    let expected = [
        (
            "root_inst",
            RestartPolicy::Always,
            RestartOrigin::LauncherRoot,
            None,
        ),
        (
            "entry_inst",
            RestartPolicy::OnFailure,
            RestartOrigin::Instance,
            std::num::NonZeroU32::new(5),
        ),
        (
            "adjusted_inst",
            RestartPolicy::OnFailure,
            RestartOrigin::Instance,
            None,
        ),
        (
            "manifest_inst",
            RestartPolicy::Never,
            RestartOrigin::Node,
            None,
        ),
        (
            "pub_inst",
            RestartPolicy::OnFailure,
            RestartOrigin::Node,
            std::num::NonZeroU32::new(2),
        ),
        (
            "pub2_inst",
            RestartPolicy::Always,
            RestartOrigin::LauncherRoot,
            None,
        ),
    ];
    for (id, policy, origin, max_restarts) in expected {
        let listed = instance(&h, id).await;
        assert_eq!(
            (
                listed.restart_status.restart.policy(),
                listed.restart_status.restart_origin
            ),
            (policy, origin),
            "{id}"
        );
        assert_eq!(
            listed.restart_status.restart.max_restarts(),
            max_restarts,
            "{id}"
        );
    }

    // The root policy covers `root_inst` and the publisher `pub2_inst`, the
    // entry's `entry_inst` and the adjustment's `adjusted_inst` over their
    // nodes' own `never`, and the publisher's own manifest `pub_inst`: a
    // crash restarts each; the
    // manifest's `never` leaves `manifest_inst` finished after its clean
    // exit, and `node stop` then clears it.
    for (node, id) in [
        ("by_root", "root_inst"),
        ("by_entry", "entry_inst"),
        ("by_adjustment", "adjusted_inst"),
        ("by_publisher", "pub_inst"),
        ("root_publisher", "pub2_inst"),
    ] {
        kill(read_pid(&pid_dir.path().join(format!("{node}.pid"))));
        wait_for_restarts(&h, id, 1).await;
    }
    terminate(read_pid(&pid_dir.path().join("by_manifest.pid")));
    wait_for_state(&h, "manifest_inst", InstanceState::Finished).await;
    let finished = instance(&h, "manifest_inst").await;
    assert_eq!(
        (
            finished.restart_status.attempt,
            finished.restart_status.next_start_ms
        ),
        (0, None)
    );
    assert_eq!(finished.restart_status.last_exit, Some(ExitCause::Code(0)));
    stop(&h, "manifest_inst");
    assert!(
        !lists(&h, "manifest_inst").await,
        "`node stop` clears a finished instance"
    );
}

/// A relaunch during the wait ends the restart: the new stack holds a fresh
/// instance under the id, with no restart behind it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relaunch_during_the_wait_leaves_no_slot() {
    let h = harness_with(parked_restart_backoff()).await;
    let nodes_dir = tempfile::tempdir().expect("nodes dir");
    let pid_dir = tempfile::tempdir().expect("pid dir");
    let pidfile = pid_dir.path().join("waiter.pid");
    install_cached_node(
        &h,
        nodes_dir.path(),
        "waiter",
        &pidfile_node("waiter", &pidfile, None),
    );
    let launcher = nodes_dir.path().join("peppy_launcher.json5");
    std::fs::write(
        &launcher,
        r#"{
            peppy_schema: "launcher/v1",
            framework: { restart: "on_failure" },
            deployments: [
                { source: { name: "waiter", tag: "v1" }, instances: [{ instance_id: "waiter_inst" }] },
            ],
        }"#,
    )
    .expect("launcher");
    h.emulate_startup("waiter", "waiter_inst").await;
    launch(&h, &launcher).expect("stack launch");

    kill(read_pid(&pidfile));
    wait_for_state(&h, "waiter_inst", InstanceState::Restarting).await;
    let run_log_before = run_log(&h, "waiter_inst");
    let err = node_run_command("waiter_inst", "waiter", Vec::new(), Vec::new())
        .execute(&h.ctx)
        .expect_err("a restarting id is taken");
    assert!(
        err.to_string().contains(&format!(
            "Instance ID `waiter_inst` already exists for node `waiter:v1` and is restarting; \
             `peppy node stop waiter_inst --core-node {}` frees the id",
            h.core_node_name
        )),
        "{err}"
    );
    assert_eq!(
        run_log(&h, "waiter_inst"),
        run_log_before,
        "the preflight refused it, and the daemon was not asked"
    );
    launch(&h, &launcher).expect("the relaunch");
    let listed = instance(&h, "waiter_inst").await;
    assert_eq!(listed.state, InstanceState::Running);
    assert_eq!(
        (
            listed.restart_status.restart_count,
            listed.restart_status.attempt
        ),
        (0, 0)
    );
    assert_eq!(listed.restart_status.last_exit, None);
    assert!(
        !stack_log(&h).contains("restarted"),
        "no restart happened:\n{}",
        stack_log(&h)
    );
}

/// A copy removed while the instance it fed waits is absent from the
/// instance's set after its restart, and the copy that stays is in it; a
/// join that grows the set during the wait is refused naming the state, and
/// succeeds once the instance runs again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_copy_removed_during_the_wait_is_absent_after_the_restart() {
    let h = harness().await;
    let nodes_dir = tempfile::tempdir().expect("nodes dir");
    let git_hash = read_daemon_git_hash(h.serve.daemon_state_path());
    let pid_dir = tempfile::tempdir().expect("pid dir");
    let dumps = tempfile::tempdir().expect("dumps dir");
    let hub_pidfile = pid_dir.path().join("hub.pid");
    // Each process of the hub copies its runtime config under its pid.
    let hub_run_cmd = dumping_run_cmd(&hub_pidfile, dumps.path());
    let hub_path = write_node_config_for_helper(
        nodes_dir.path(),
        "hub",
        TAG,
        &git_hash,
        &hub_run_cmd,
        Some(
            r#"{ nodes: [{ name: "arm", tag: "v1", link_id: "arms", cardinality: "zero_or_more" }] }"#,
        ),
        None,
        None,
    );
    let arm_path = install_node_manifest(
        nodes_dir.path(),
        "arm",
        &git_hash,
        &per_instance_pidfile_node("arm", pid_dir.path(), None),
    );
    let arm_pid = |instance_id: &str| pid_in(pid_dir.path(), instance_id);
    register_repo_caches(
        h.serve.temp_dir(),
        &[("hub", TAG, &hub_path), ("arm", TAG, &arm_path)],
    );
    let launcher = nodes_dir.path().join("peppy_launcher.json5");
    std::fs::write(
        &launcher,
        r#"{
            peppy_schema: "launcher/v1",
            components: [{ name: "robot", cardinality: "zero_or_more", options: { real: {
                deployments: [{ source: { name: "arm", tag: "v1" },
                                instances: [{ instance_id: "arm_inst", framework: { restart: "on_failure" } }] }],
                adjustments: [{ target: "hub_inst", add_links: { arms: ["arm_inst"] } }],
            } } }],
            deployments: [
                { source: { name: "hub", tag: "v1" },
                  instances: [{ instance_id: "hub_inst", framework: { restart: "on_failure" } }] },
                { robot: "real", instances: [{ instance_id: "alpha" }, { instance_id: "bravo" }] },
            ],
        }"#,
    )
    .expect("launcher");
    h.emulate_startup("hub", "hub_inst").await;
    h.emulate_startup("arm", "alpha_arm_inst").await;
    h.emulate_startup("arm", "bravo_arm_inst").await;
    emulate_cooperative_shutdown(
        &h.messenger,
        &h.core_node_name,
        "arm",
        "alpha_arm_inst",
        pid_dir.path().join("alpha_arm_inst.pid"),
    )
    .await;
    let (arms_tx, mut arms_rx) = watch::channel(BoundSetState::seeded(
        BoundProducers::try_from(Vec::<config::runtime::ProducerRef>::new()).expect("an empty set"),
    ));
    let _binding_mock = listen_for_binding_update(
        &h.messenger,
        &h.core_node_name,
        "hub_inst",
        test_node_target("hub"),
        Arc::new(BTreeMap::from([(
            "arms".to_owned(),
            SlotChannel::new(Cardinality::ZeroOrMore, arms_tx),
        )])),
    )
    .await
    .expect("binding_update mock");
    launch(&h, &launcher).expect("stack launch");
    assert_eq!(arms_of(&h).await, ["alpha_arm_inst", "bravo_arm_inst"]);

    // Restarts are held off while the test acts on the waiting instance, as
    // a removal holds them, so the wait lasts as long as the test needs.
    let hold = h.serve.hold_restarts();
    kill(read_pid(&hub_pidfile));
    wait_for_state(&h, "hub_inst", InstanceState::Restarting).await;
    let err = join(&h, "real", "charlie").expect_err("joining grows a restarting instance's set");
    assert!(
        err.to_string().contains("but `hub_inst` is restarting on")
            && err.to_string().contains(
                "; join again once it is running, or join an option whose fragments do not add \
                 to that slot"
            ),
        "{err}"
    );
    assert_daemon_failure(
        &err,
        PeppyFailureKind::PreconditionFailed,
        "is restarting on",
    );
    remove_copy(&h, "alpha").await;
    assert_eq!(
        instance(&h, "hub_inst").await.state,
        InstanceState::Restarting,
        "the removal happened during the wait"
    );
    assert!(
        !lists(&h, "alpha_arm_inst").await,
        "the removal ended the copy's arm"
    );
    assert_eq!(
        arms_of(&h).await,
        ["bravo_arm_inst"],
        "the slot forgets the copy"
    );

    let first_hub = read_pid(&hub_pidfile);
    drop(hold);
    wait_for_restarts(&h, "hub_inst", 1).await;
    assert_eq!(arms_of(&h).await, ["bravo_arm_inst"]);
    let second_hub = wait_for_new_pid(&h, &hub_pidfile, first_hub).await;
    let arms_in_config = |pid: u32| -> Vec<String> {
        let arms =
            dumped_config(dumps.path(), pid)["node_instance"]["slot_bindings"]["arms"].clone();
        serde_json::from_value::<BoundProducers>(arms)
            .expect("the hub's runtime config binds its arms")
            .iter()
            .map(|member| member.producer.instance_id.clone())
            .collect()
    };
    assert_eq!(
        arms_in_config(first_hub),
        ["alpha_arm_inst", "bravo_arm_inst"],
        "the first hub started bound to both copies"
    );
    assert_eq!(
        arms_in_config(second_hub),
        ["bravo_arm_inst"],
        "the restarted hub starts from the shrunk set"
    );
    // The respawn holds the stack's guard until its start returns, a moment
    // after the listing shows the hub running; a join meanwhile answers
    // busy, as under any start, and is tried again.
    h.emulate_startup("arm", "charlie_arm_inst").await;
    wait_until(&h, "a join once the hub runs again", async || {
        match join(&h, "real", "charlie") {
            Ok(()) => true,
            Err(e) if e.to_string().contains("in progress on this daemon") => false,
            Err(e) => panic!("the join is refused: {e}"),
        }
    })
    .await;
    assert_eq!(arms_of(&h).await, ["bravo_arm_inst", "charlie_arm_inst"]);
    let delivered: Vec<String> = arms_rx
        .borrow_and_update()
        .producers
        .iter()
        .map(|member| member.producer.instance_id.clone())
        .collect();
    assert_eq!(
        delivered,
        ["bravo_arm_inst", "charlie_arm_inst"],
        "the hub took the grown set"
    );

    // The fragment's entry gave the copy's arm its policy: a crash restarts
    // it. A copy removed while its own instance is restarting is gone with
    // the instance, and no restart follows.
    let charlie = instance(&h, "charlie_arm_inst").await;
    assert_eq!(
        (
            charlie.restart_status.restart.policy(),
            charlie.restart_status.restart_origin
        ),
        (RestartPolicy::OnFailure, RestartOrigin::Instance)
    );
    let first_charlie = arm_pid("charlie_arm_inst");
    kill(first_charlie);
    wait_for_restarts(&h, "charlie_arm_inst", 1).await;
    let second_charlie = wait_for_new_pid(
        &h,
        &pid_dir.path().join("charlie_arm_inst.pid"),
        first_charlie,
    )
    .await;
    let hold = h.serve.hold_restarts();
    kill(second_charlie);
    wait_for_state(&h, "charlie_arm_inst", InstanceState::Restarting).await;
    remove_copy(&h, "charlie").await;
    drop(hold);
    assert!(
        !lists(&h, "charlie_arm_inst").await,
        "the removed copy's restarting instance is gone"
    );
    assert_eq!(arms_of(&h).await, ["bravo_arm_inst"]);
    assert_eq!(
        stack_log(&h).matches("restarted (attempt").count(),
        2,
        "the hub's and charlie's one restart each:\n{}",
        stack_log(&h)
    );
}

/// Whether the stack lists `instance_id`, in any state.
async fn lists(h: &Harness, instance_id: &str) -> bool {
    graph(h)
        .await
        .nodes
        .iter()
        .flat_map(|node| &node.instances)
        .any(|instance| instance.instance_id == instance_id)
}

/// The run command of a node each of whose processes records its pid to
/// `pidfile` and copies its runtime config under its pid into `dumps`.
fn dumping_run_cmd(pidfile: &Path, dumps: &Path) -> Vec<String> {
    vec![
        "sh".to_owned(),
        "-c".to_owned(),
        format!(
            "echo $$ > '{}'; {}; exec sleep 300",
            pidfile.display(),
            dump_runtime_config(dumps)
        ),
    ]
}

/// The runtime config the process `pid` copied under `dumps`.
fn dumped_config(dumps: &Path, pid: u32) -> serde_json::Value {
    poll_file(&dumps.join(format!("{pid}.json5")), |text| {
        serde_json5::from_str(text).ok()
    })
}

/// The members of `hub_inst`'s `arms` slot as the stack lists them.
async fn arms_of(h: &Harness) -> Vec<String> {
    instance(h, "hub_inst")
        .await
        .slot_bindings
        .get("arms")
        .map(|producers| {
            producers
                .iter()
                .map(|member| member.producer.instance_id.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// An arm `arm_1` running vacant and a controller `ctrl_1` paired to it,
/// both under `on_failure`, with the watches reading their pairing slots.
/// Every instance records its pid under its id in `pid_dir`.
struct PairedRig {
    pid_dir: tempfile::TempDir,
    arm_rx: watch::Receiver<peppylib::messaging::PeerSetState>,
    ctrl_rx: watch::Receiver<peppylib::messaging::PeerSetState>,
    _dirs: Vec<tempfile::TempDir>,
}

/// The pid the instance `instance_id` of a rig recorded under `pid_dir`.
fn pid_in(pid_dir: &Path, instance_id: &str) -> u32 {
    read_pid(&pid_dir.join(format!("{instance_id}.pid")))
}

async fn paired_rig(h: &Harness, instances: &InstanceLifetime) -> PairedRig {
    let pid_dir = tempfile::tempdir().expect("pid dir");
    let pidfile_word = per_instance_pidfile_word(pid_dir.path());
    let repo_dir = tempfile::tempdir().expect("repo dir");
    seed_pairing_repo(&h.serve, &h.ctx, repo_dir.path());
    let arm_dir = tempfile::tempdir().expect("arm dir");
    add_built_node(
        &h.ctx,
        arm_dir.path(),
        &pairing_node_config_writing_pid_to(
            "robot_arm",
            "arm",
            "controller",
            Cardinality::ZeroOrOne,
            instances,
            &pidfile_word,
        ),
    );
    let ctrl_dir = tempfile::tempdir().expect("controller dir");
    add_built_node(
        &h.ctx,
        ctrl_dir.path(),
        &pairing_node_config_writing_pid_to(
            "arm_controller",
            "controller",
            "arm",
            Cardinality::ZeroOrOne,
            instances,
            &pidfile_word,
        ),
    );

    let mut arm_rx = emulate_arm(h, pid_dir.path()).await;
    run_with_vacant_link(h, "arm_1", "robot_arm", "controller");
    let mut ctrl_rx = run_controller(h, "ctrl_1", pid_dir.path()).await;
    assert_eq!(peer_of(&mut arm_rx), Some("ctrl_1".to_string()));
    assert_eq!(peer_of(&mut ctrl_rx), Some("arm_1".to_string()));
    PairedRig {
        pid_dir,
        arm_rx,
        ctrl_rx,
        _dirs: vec![repo_dir, arm_dir, ctrl_dir],
    }
}

/// Runs `instance_id` of `node` under `on_failure` with its `link_id`
/// pairing slot vacant.
fn run_with_vacant_link(h: &Harness, instance_id: &str, node: &str, link_id: &str) {
    run_with_vacant_link_under(h, instance_id, node, link_id, RestartPolicy::OnFailure);
}

/// [`run_with_vacant_link`] under `policy`.
fn run_with_vacant_link_under(
    h: &Harness,
    instance_id: &str,
    node: &str,
    link_id: &str,
    policy: RestartPolicy,
) {
    let mut run = run_with_restart(instance_id, node, Some(policy), None);
    if let NodeCommands::Run { vacant_links, .. } = &mut run.command {
        vacant_links.push((
            link_id.to_string(),
            daemon_config::launcher::VacantReason::new("test rig: paired later").unwrap(),
        ));
    }
    run.execute(&h.ctx)
        .unwrap_or_else(|e| panic!("`{instance_id}` runs vacant: {e}"));
}

/// Runs the controller `instance_id` under `on_failure`, paired to `arm_1`,
/// with its services emulated; returns the watch reading its slot.
async fn run_controller(
    h: &Harness,
    instance_id: &str,
    pid_dir: &Path,
) -> watch::Receiver<peppylib::messaging::PeerSetState> {
    let ctrl_rx = emulate_controller(h, instance_id, pid_dir).await;
    controller_link_run(instance_id)
        .execute(&h.ctx)
        .unwrap_or_else(|e| panic!("`{instance_id}` runs paired: {e}"));
    ctrl_rx
}

/// The services of the controller `instance_id`, emulated; returns the
/// watch reading its slot.
async fn emulate_controller(
    h: &Harness,
    instance_id: &str,
    pid_dir: &Path,
) -> watch::Receiver<peppylib::messaging::PeerSetState> {
    emulate_pairing_instance(
        &h.messenger,
        &h.core_node_name,
        "arm_controller",
        instance_id,
        "arm",
        Cardinality::ZeroOrOne,
        &pid_dir.join(format!("{instance_id}.pid")),
    )
    .await
}

/// The run that pairs the controller `instance_id` to `arm_1` under
/// `on_failure`.
fn controller_link_run(instance_id: &str) -> NodeCommand {
    let mut run = run_with_restart(
        instance_id,
        "arm_controller",
        Some(RestartPolicy::OnFailure),
        None,
    );
    if let NodeCommands::Run { links, .. } = &mut run.command {
        links.push(("arm".to_string(), "arm_1".to_string()));
    }
    run
}

/// Two paired instances exchange their pins again after either is killed
/// and restarted: the restarted one requests the pair it held, and the
/// survivor is pinned to it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn re_pair_after_either_endpoint_restarts() {
    let h = harness().await;
    let instances = InstanceLifetime::new();
    let mut rig = paired_rig(&h, &instances).await;
    let pid_dir = rig.pid_dir.path().to_path_buf();
    let PairedRig {
        arm_rx, ctrl_rx, ..
    } = &mut rig;

    // The controller, the requester of the pair, dies: its exit dissolves
    // the pair, and the restarted controller requests it again, so both
    // watches hold the pins and the stack lists the pair.
    let first_arm = pid_in(&pid_dir, "arm_1");
    kill(pid_in(&pid_dir, "ctrl_1"));
    wait_for_restarts(&h, "ctrl_1", 1).await;
    wait_until(
        &h,
        "the pair is made again after the controller's restart",
        async || paired_again(&h, arm_rx, ctrl_rx).await,
    )
    .await;

    // The arm, the covered endpoint, dies: the restarted arm requests the
    // pair it held, so the controller is pinned to it again.
    kill(first_arm);
    wait_for_restarts(&h, "arm_1", 1).await;
    wait_until(
        &h,
        "the pair is made again after the arm's restart",
        async || paired_again(&h, arm_rx, ctrl_rx).await,
    )
    .await;
    wait_until(
        &h,
        "the restarted arm logs the pair it made again",
        async || run_log(&h, "arm_1").contains("paired:"),
    )
    .await;

    // A deliberate stop of the controller forgets the pair on the arm: with
    // a controller started again vacant under the same id, the arm restarted
    // after that requests nothing and the two stay unpaired.
    stop(&h, "ctrl_1");
    wait_until(&h, "the survivor is unpaired", async || {
        peer_of(arm_rx).is_none()
    })
    .await;
    run_with_vacant_link(&h, "ctrl_1", "arm_controller", "arm");
    let arm_pidfile = pid_dir.join("arm_1.pid");
    kill(wait_for_new_pid(&h, &arm_pidfile, first_arm).await);
    wait_for_restarts(&h, "arm_1", 2).await;
    // Past the pair requests the restarted arm would make once running.
    tokio::time::sleep(QUIET).await;
    let after_second_restart = restarted_run_log(&h, "arm_1");
    assert!(
        !after_second_restart.contains("paired:") && !lists_pair_with(&h, "ctrl_1").await,
        "a pair with a stopped peer is not made again:\n{after_second_restart}"
    );
}

/// Whether `arm_1` and `ctrl_1` are paired again: each watch holds the
/// other, and the stack lists the pair.
async fn paired_again(
    h: &Harness,
    arm_rx: &mut watch::Receiver<peppylib::messaging::PeerSetState>,
    ctrl_rx: &mut watch::Receiver<peppylib::messaging::PeerSetState>,
) -> bool {
    peer_of(arm_rx) == Some("ctrl_1".to_string())
        && peer_of(ctrl_rx) == Some("arm_1".to_string())
        && lists_pair_with(h, "ctrl_1").await
}

/// Whether `stack list` shows the arm paired with the controller `ctrl`.
async fn lists_pair_with(h: &Harness, ctrl: &str) -> bool {
    peppy::commands::stack::list_nodes_collecting(&h.ctx, false, None)
        .await
        .expect("stack list")
        .output
        .contains(&format!(
            "controller ⇌ {ctrl}:arm@{} (arm_link:v1)",
            h.core_node_name
        ))
}

/// A held pair whose peer slot another instance took during the wait is
/// forgotten on both ends when the slot is taken: the restart requests
/// nothing of it, and the arm keeps the controller it took last.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_pair_whose_peer_slot_was_taken_is_forgotten() {
    let h = harness().await;
    let instances = InstanceLifetime::new();
    let mut rig = paired_rig(&h, &instances).await;
    let pid_dir = rig.pid_dir.path().to_path_buf();
    let PairedRig { arm_rx, .. } = &mut rig;

    // The controller dies; its exit frees the arm's scalar slot, which a
    // second controller takes during the wait, held open by the test.
    let hold = h.serve.hold_restarts();
    kill(pid_in(&pid_dir, "ctrl_1"));
    wait_for_state(&h, "ctrl_1", InstanceState::Restarting).await;
    wait_until(&h, "the arm is unpaired", async || {
        peer_of(arm_rx).is_none()
    })
    .await;
    let mut ctrl_2_rx = run_controller(&h, "ctrl_2", &pid_dir).await;
    assert_eq!(peer_of(arm_rx), Some("ctrl_2".to_string()));
    assert_eq!(peer_of(&mut ctrl_2_rx), Some("arm_1".to_string()));

    drop(hold);
    wait_for_restarts(&h, "ctrl_1", 1).await;
    // Past the pair requests the restarted controller would make.
    tokio::time::sleep(QUIET).await;
    let after_restart = restarted_run_log(&h, "ctrl_1");
    assert!(
        !after_restart.contains("arm_1"),
        "the restart asks nothing of the taken slot:\n{after_restart}"
    );
    assert!(
        lists_pair_with(&h, "ctrl_2").await && !lists_pair_with(&h, "ctrl_1").await,
        "the second controller keeps the arm"
    );
    assert_eq!(peer_of(arm_rx), Some("ctrl_2".to_string()));

    // The arm's restart pairs the second controller and asks nothing of the
    // first.
    kill(pid_in(&pid_dir, "arm_1"));
    wait_for_restarts(&h, "arm_1", 1).await;
    wait_until(
        &h,
        "the restarted arm pairs the second controller",
        async || peer_of(arm_rx) == Some("ctrl_2".to_string()),
    )
    .await;
    let after_restart = restarted_run_log(&h, "arm_1");
    assert!(
        !after_restart.contains("ctrl_1"),
        "the restarted arm requests no pair with the first controller:\n{after_restart}"
    );
    assert!(lists_pair_with(&h, "ctrl_2").await && !lists_pair_with(&h, "ctrl_1").await);
}

/// Two paired instances killed before either runs again both come back and
/// are paired once: the restart that finds its peer running requests the
/// pair it held, and the stack holds one pair.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn re_pair_after_both_restart() {
    let h = harness().await;
    let instances = InstanceLifetime::new();
    let mut rig = paired_rig(&h, &instances).await;
    let pid_dir = rig.pid_dir.path().to_path_buf();
    let PairedRig {
        arm_rx, ctrl_rx, ..
    } = &mut rig;

    // Both exits land before either restart, held off by the test.
    let hold = h.serve.hold_restarts();
    kill(pid_in(&pid_dir, "arm_1"));
    kill(pid_in(&pid_dir, "ctrl_1"));
    wait_for_state(&h, "arm_1", InstanceState::Restarting).await;
    wait_for_state(&h, "ctrl_1", InstanceState::Restarting).await;
    assert!(!lists_pair_with(&h, "ctrl_1").await, "the pair dissolved");
    drop(hold);
    wait_for_restarts(&h, "arm_1", 1).await;
    wait_for_restarts(&h, "ctrl_1", 1).await;
    wait_until(&h, "the pair is made again once both run", async || {
        paired_again(&h, arm_rx, ctrl_rx).await
    })
    .await;
    let listing = peppy::commands::stack::list_nodes_collecting(&h.ctx, false, None)
        .await
        .expect("stack list")
        .output;
    assert_eq!(
        listing.matches("controller ⇌ ctrl_1:arm@").count(),
        1,
        "one pair, listed once:\n{listing}"
    );
    for id in ["arm_1", "ctrl_1"] {
        assert!(
            !restarted_run_log(&h, id).contains("not made again"),
            "a restart whose peer is not running yet leaves the pair to the peer's own:\n{}",
            run_log(&h, id)
        );
    }
}

/// The object of `instance_id` in a `peppy stack list --json` document.
fn listed_instance(document: &serde_json::Value, instance_id: &str) -> Option<serde_json::Value> {
    match document {
        serde_json::Value::Object(fields)
            if fields
                .get("instance_id")
                .and_then(serde_json::Value::as_str)
                == Some(instance_id) =>
        {
            Some(document.clone())
        }
        serde_json::Value::Object(fields) => fields
            .values()
            .find_map(|value| listed_instance(value, instance_id)),
        serde_json::Value::Array(items) => items
            .iter()
            .find_map(|value| listed_instance(value, instance_id)),
        _ => None,
    }
}

/// A pairing slot of a restarting instance is not live: a run that links
/// into it is refused naming the instance, and succeeds once the instance
/// runs again under the id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_run_link_into_a_restarting_slot_is_refused() {
    let h = harness_with(parked_restart_backoff()).await;
    let instances = InstanceLifetime::new();
    let rig = paired_rig(&h, &instances).await;
    let pid_dir = rig.pid_dir.path().to_path_buf();
    stop(&h, "ctrl_1");
    kill(pid_in(&pid_dir, "arm_1"));
    wait_for_state(&h, "arm_1", InstanceState::Restarting).await;
    // `node info` counts the hour's wait down on the daemon's clock.
    let info = node_info(&h, "robot_arm").await;
    let next_start_secs: u64 = info
        .split_once("next start in ")
        .and_then(|(_, rest)| rest.split_once(" s"))
        .and_then(|(secs, _)| secs.parse().ok())
        .unwrap_or_else(|| panic!("a restarting instance names its next start: {info}"));
    assert!(
        (3_500..=3_600).contains(&next_start_secs),
        "{next_start_secs} s of an hour's wait: {info}"
    );
    assert!(
        info.contains("[restarting]")
            && info.contains("attempt 1, last exit: signal 9 (SIGKILL), next start in"),
        "the restarting instance's line names its attempt and last exit: {info}"
    );
    // `peppy stack list --json` carries the restart fields of a restarting
    // instance.
    let listed = peppy::commands::stack::list_nodes_json_collecting(&h.ctx)
        .await
        .expect("stack list --json")
        .output;
    let document: serde_json::Value = serde_json::from_str(&listed).expect("a JSON document");
    let arm = listed_instance(&document, "arm_1")
        .unwrap_or_else(|| panic!("`arm_1` is listed: {listed}"));
    // The cap travels inside `restart`'s object form.
    for key in [
        "restart",
        "restart_origin",
        "restart_count",
        "attempt",
        "last_exit",
        "running_since_ms",
        "next_start_ms",
    ] {
        assert!(arm.get(key).is_some(), "`{key}` is listed: {arm}");
    }
    assert_eq!(arm["state"], "restarting", "{arm}");
    assert_eq!(arm["restart_count"], 0, "{arm}");
    assert_eq!(arm["attempt"], 1, "{arm}");
    assert_eq!(arm["last_exit"]["signal"]["number"], 9, "{arm}");
    assert!(arm["running_since_ms"].is_u64(), "{arm}");
    assert!(arm["next_start_ms"].is_u64(), "{arm}");
    let _ctrl_2_rx = emulate_controller(&h, "ctrl_2", &pid_dir).await;
    let err = controller_link_run("ctrl_2")
        .execute(&h.ctx)
        .expect_err("a restarting slot takes no pair");
    assert!(
        err.to_string().contains(
            "link `arm` names instance `arm_1`, which is restarting; link to it once it is \
             running"
        ),
        "{err}"
    );
    assert!(!lists_pair_with(&h, "ctrl_2").await);

    // A failed arm is refused too, naming the stop that clears it.
    let stopped_arm = pid_in(&pid_dir, "arm_1");
    stop(&h, "arm_1");
    let _arm_rx = emulate_arm(&h, &pid_dir).await;
    run_with_vacant_link_under(&h, "arm_1", "robot_arm", "controller", RestartPolicy::Never);
    kill(wait_for_new_pid(&h, &pid_dir.join("arm_1.pid"), stopped_arm).await);
    wait_for_state(&h, "arm_1", InstanceState::Failed).await;
    let err = controller_link_run("ctrl_2")
        .execute(&h.ctx)
        .expect_err("a failed slot takes no pair");
    assert!(
        err.to_string().contains(&format!(
            "link `arm` names instance `arm_1`, which is failed; `peppy node stop arm_1 \
             --core-node {}` clears it; start it again as it was started, then link to it",
            h.core_node_name
        )),
        "{err}"
    );

    stop(&h, "arm_1");
    let _arm_rx = emulate_arm(&h, &pid_dir).await;
    run_with_vacant_link(&h, "arm_1", "robot_arm", "controller");
    controller_link_run("ctrl_2")
        .execute(&h.ctx)
        .expect("the arm runs again under its id, so the link is made");
    assert!(lists_pair_with(&h, "ctrl_2").await);
}

/// The services of the arm `arm_1` of a rig, emulated; returns the watch
/// reading its slot.
async fn emulate_arm(
    h: &Harness,
    pid_dir: &Path,
) -> watch::Receiver<peppylib::messaging::PeerSetState> {
    emulate_pairing_instance(
        &h.messenger,
        &h.core_node_name,
        "robot_arm",
        "arm_1",
        "controller",
        Cardinality::ZeroOrOne,
        &pid_dir.join("arm_1.pid"),
    )
    .await
}

/// The peer a slot's watch currently holds.
fn peer_of(rx: &mut watch::Receiver<peppylib::messaging::PeerSetState>) -> Option<String> {
    rx.borrow_and_update()
        .peers()
        .next()
        .map(|peer| peer.producer.instance_id.clone())
}

/// An observer of a restarted source receives the source's new generation,
/// and a restarted observer receives its sources again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_observer_sees_the_new_generation_after_a_restart() {
    let h = harness().await;
    let instances = InstanceLifetime::new();
    let pid_dir = tempfile::tempdir().expect("pid dir");
    let source_pidfile = pid_dir.path().join("source.pid");
    let repo_dir = tempfile::tempdir().expect("repo dir");
    seed_pairing_repo(&h.serve, &h.ctx, repo_dir.path());
    let source_dir = tempfile::tempdir().expect("source dir");
    add_built_node(
        &h.ctx,
        source_dir.path(),
        &pairing_node_config(
            "robot_arm",
            "arm",
            "controller",
            config::node::Cardinality::ZeroOrOne,
            &instances,
            &source_pidfile,
        ),
    );
    let observer_pidfile = pid_dir.path().join("observer.pid");
    let observer_dir = tempfile::tempdir().expect("observer dir");
    add_built_node(
        &h.ctx,
        observer_dir.path(),
        &format!(
            r#"{{
                peppy_schema: "node/v1",
                manifest: {{
                    name: "recorder",
                    tag: "v1",
                    depends_on: {{
                        pairing_observers: [
                            {{ name: "arm_link", tag: "v1", role: "arm", link_id: "watch", cardinality: "zero_or_one" }}
                        ]
                    }}
                }},
                interfaces: {{ topics: {{ consumes: [{{ link_id: "watch", name: "joint_states" }}] }} }},
                execution: {{ language: "rust", run_cmd: ["sh", "-c", "echo $$ > '{}'; exec sleep 300"] }}
            }}"#,
            observer_pidfile.display()
        ),
    );

    h.emulate_startup("robot_arm", "arm_1").await;
    run_with_vacant_link(&h, "arm_1", "robot_arm", "controller");

    h.emulate_startup("recorder", "rec_1").await;
    let (slots, mut receivers) = super::common::observer_slot_channels(&["watch"]);
    let obs_rx = receivers.remove("watch").expect("the slot's watch");
    let _observation_mock = listen_for_observation_update(
        &h.messenger,
        &h.core_node_name,
        "rec_1",
        test_node_target("recorder"),
        slots,
    )
    .await
    .expect("observation_update mock");
    let mut run = run_with_restart("rec_1", "recorder", Some(RestartPolicy::OnFailure), None);
    if let NodeCommands::Run { links, .. } = &mut run.command {
        links.push(("watch".to_string(), "arm_1".to_string()));
    }
    run.execute(&h.ctx).expect("the observer runs");
    let first = member(&obs_rx);
    assert!(first.source_live);

    // The source restarts under a new generation.
    kill(read_pid(&source_pidfile));
    wait_for_restarts(&h, "arm_1", 1).await;
    wait_until(&h, "the observer sees the new generation", async || {
        let now = member(&obs_rx);
        now.source_live && now.source_generation > first.source_generation
    })
    .await;

    // The observer restarts and receives its source again: a delivery
    // after its restart, with the generation the source runs under.
    let generation = member(&obs_rx).source_generation;
    let delivered_before = obs_rx.borrow().sequence;
    kill(read_pid(&observer_pidfile));
    wait_for_restarts(&h, "rec_1", 1).await;
    wait_until(
        &h,
        "the restarted observer receives its source",
        async || {
            let now = member(&obs_rx);
            obs_rx.borrow().sequence > delivered_before
                && now.source_live
                && now.source_generation == generation
        },
    )
    .await;
    assert_eq!(
        member(&obs_rx).source.producer.instance_id,
        "arm_1",
        "the restarted observer observes the source it had"
    );
}

/// A restarted instance reads the clock domain it read before: the runtime
/// config of its second process carries the binding of the first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_clock_consumer_reads_its_domain_again() {
    let h = harness().await;
    let nodes_dir = tempfile::tempdir().expect("nodes dir");
    let git_hash = read_daemon_git_hash(h.serve.daemon_state_path());
    let pid_dir = tempfile::tempdir().expect("pid dir");
    let dump_dir = tempfile::tempdir().expect("dump dir");
    let pub_pidfile = pid_dir.path().join("by_publisher.pid");
    let reader_pidfile = pid_dir.path().join("reader.pid");
    let pub_path = install_node_manifest(
        nodes_dir.path(),
        "by_publisher",
        &git_hash,
        &pidfile_node("by_publisher", &pub_pidfile, None),
    );
    // Each process of the reader copies its runtime config under its pid.
    let reader_path = super::common::write_minimal_node_config(
        nodes_dir.path(),
        "reader",
        TAG,
        &git_hash,
        &dumping_run_cmd(&reader_pidfile, dump_dir.path()),
    );
    register_repo_caches(
        h.serve.temp_dir(),
        &[
            ("by_publisher", TAG, &pub_path),
            ("reader", TAG, &reader_path),
        ],
    );
    let launcher = nodes_dir.path().join("peppy_launcher.json5");
    std::fs::write(
        &launcher,
        r#"{
            peppy_schema: "launcher/v1",
            framework: { restart: "on_failure", clocks: { sim: { publisher: "pub_inst" } } },
            deployments: [
                { source: { name: "by_publisher", tag: "v1" }, instances: [{ instance_id: "pub_inst" }] },
                { source: { name: "reader", tag: "v1" },
                  instances: [{ instance_id: "reader_inst", framework: { clock: "sim" } }] },
            ],
        }"#,
    )
    .expect("launcher");
    h.emulate_startup("by_publisher", "pub_inst").await;
    h.emulate_startup("reader", "reader_inst").await;
    launch(&h, &launcher).expect("stack launch");

    let clock_of = |pid: u32| {
        dumped_config(dump_dir.path(), pid)["node_instance"]["framework"]["clock"].clone()
    };
    let first_pid = read_pid(&reader_pidfile);
    let first_clock = clock_of(first_pid);
    assert!(
        first_clock.to_string().contains("sim"),
        "the reader is bound to the domain: {first_clock}"
    );

    kill(first_pid);
    wait_for_restarts(&h, "reader_inst", 1).await;
    let second_pid = wait_for_new_pid(&h, &reader_pidfile, first_pid).await;
    assert_eq!(
        clock_of(second_pid),
        first_clock,
        "the restarted reader reads the same domain under the same incarnation"
    );
}

/// A restarted observer receives the sources it observed: one a join added
/// before its exit stays, one a removal took away while it waited is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_observer_set_shrunk_during_the_wait_is_in_the_respawn() {
    let h = harness().await;
    let nodes_dir = tempfile::tempdir().expect("nodes dir");
    let git_hash = read_daemon_git_hash(h.serve.daemon_state_path());
    let pid_dir = tempfile::tempdir().expect("pid dir");
    let repo_dir = tempfile::tempdir().expect("repo dir");
    seed_pairing_repo(&h.serve, &h.ctx, repo_dir.path());
    let rec_pidfile = pid_dir.path().join("recorder.pid");
    let dumps = tempfile::tempdir().expect("dumps dir");
    let arm_path = install_node_manifest(
        nodes_dir.path(),
        "robot_arm",
        &git_hash,
        &observed_arm_manifest(pid_dir.path()),
    );
    // Each process of the recorder copies its runtime config under its pid.
    let rec_run_cmd = dumping_run_cmd(&rec_pidfile, dumps.path());
    let rec_path = write_node_config_for_helper(
        nodes_dir.path(),
        "recorder",
        TAG,
        &git_hash,
        &rec_run_cmd,
        Some(
            r#"{ pairing_observers: [
                { name: "arm_link", tag: "v1", role: "arm", link_id: "watch", cardinality: "zero_or_more" }
            ] }"#,
        ),
        None,
        Some(r#"{ topics: { consumes: [{ link_id: "watch", name: "joint_states" }] } }"#),
    );
    register_repo_caches(
        h.serve.temp_dir(),
        &[("robot_arm", TAG, &arm_path), ("recorder", TAG, &rec_path)],
    );
    let launcher = nodes_dir.path().join("peppy_launcher.json5");
    std::fs::write(
        &launcher,
        r#"{
            peppy_schema: "launcher/v1",
            components: [{ name: "robot", cardinality: "zero_or_more", options: { real: {
                deployments: [{ source: { name: "robot_arm", tag: "v1" },
                                instances: [{ instance_id: "arm_inst",
                                              links: { controller: { vacant: "test rig: no controller" } } }] }],
                adjustments: [{ target: "rec_inst", add_links: { watch: ["arm_inst"] } }],
            } } }],
            deployments: [
                { source: { name: "recorder", tag: "v1" },
                  instances: [{ instance_id: "rec_inst", framework: { restart: "on_failure" } }] },
                { robot: "real", instances: [{ instance_id: "alpha" }] },
            ],
        }"#,
    )
    .expect("launcher");
    for arm in ["alpha_arm_inst", "bravo_arm_inst"] {
        h.emulate_startup("robot_arm", arm).await;
        emulate_cooperative_shutdown(
            &h.messenger,
            &h.core_node_name,
            "robot_arm",
            arm,
            pid_dir.path().join(format!("{arm}.pid")),
        )
        .await;
    }
    h.emulate_startup("recorder", "rec_inst").await;
    let (slots, mut receivers) = super::common::observer_slot_channels(&["watch"]);
    let obs_rx = receivers.remove("watch").expect("the slot's watch");
    let _observation_mock = listen_for_observation_update(
        &h.messenger,
        &h.core_node_name,
        "rec_inst",
        test_node_target("recorder"),
        slots,
    )
    .await
    .expect("observation_update mock");
    launch(&h, &launcher).expect("stack launch");
    wait_until(&h, "the recorder observes alpha", async || {
        members(&obs_rx) == ["alpha_arm_inst"]
    })
    .await;
    join(&h, "real", "bravo").expect("stack join real:bravo");
    wait_until(&h, "the recorder observes alpha and bravo", async || {
        members(&obs_rx) == ["alpha_arm_inst", "bravo_arm_inst"]
    })
    .await;
    let delivered_before = obs_rx.borrow().sequence;

    let first_rec = read_pid(&rec_pidfile);
    let hold = h.serve.hold_restarts();
    kill(first_rec);
    wait_for_state(&h, "rec_inst", InstanceState::Restarting).await;
    remove_copy(&h, "alpha").await;
    assert_eq!(
        instance(&h, "rec_inst").await.state,
        InstanceState::Restarting,
        "the removal happened during the wait"
    );

    drop(hold);
    wait_for_restarts(&h, "rec_inst", 1).await;
    wait_until(
        &h,
        "the restarted recorder observes bravo alone",
        async || {
            obs_rx.borrow().sequence > delivered_before && members(&obs_rx) == ["bravo_arm_inst"]
        },
    )
    .await;
    // The new process starts from the records its exit took, as the
    // removal shrank them: its setup-time seed names bravo alone.
    let second_rec = wait_for_new_pid(&h, &rec_pidfile, first_rec).await;
    let seed =
        dumped_config(dumps.path(), second_rec)["node_instance"]["observation_seeds"]["watch"]
            .to_string();
    assert!(
        seed.contains("bravo_arm_inst") && !seed.contains("alpha_arm_inst"),
        "the restarted recorder's seed: {seed}"
    );
}

/// A process that exits during its first start fails the launch under
/// every policy, and no restart is scheduled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_start_failure_fails_the_launch_under_every_policy() {
    let h = harness().await;
    let nodes_dir = tempfile::tempdir().expect("nodes dir");
    let git_hash = read_daemon_git_hash(h.serve.daemon_state_path());
    let path = super::common::write_minimal_node_config(
        nodes_dir.path(),
        "exits",
        TAG,
        &git_hash,
        &["sh", "-c", "exit 3"],
    );
    register_repo_caches(h.serve.temp_dir(), &[("exits", TAG, &path)]);
    for policy in ["never", "on_failure", "always"] {
        let launcher = nodes_dir.path().join(format!("{policy}.json5"));
        std::fs::write(
            &launcher,
            format!(
                r#"{{
                    peppy_schema: "launcher/v1",
                    framework: {{ restart: "{policy}" }},
                    deployments: [
                        {{ source: {{ name: "exits", tag: "v1" }}, instances: [{{ instance_id: "exits_inst" }}] }},
                    ],
                }}"#
            ),
        )
        .expect("launcher");
        let err = launch(&h, &launcher).expect_err("a first start that fails fails the launch");
        assert_daemon_failure(
            &err,
            PeppyFailureKind::AttemptFailed,
            "exited during startup",
        );
        assert!(
            !stack_log(&h).contains("restarting"),
            "{policy}: no restart is scheduled:\n{}",
            stack_log(&h)
        );
        assert!(
            !lists(&h, "exits_inst").await,
            "{policy}: the failed start leaves no instance"
        );
    }
}

/// An arm with a vacatable controller slot whose process records its pid
/// under its instance id, the name of its working directory, so each copy's
/// instance is ended by its own shutdown mock.
fn observed_arm_manifest(pid_dir: &Path) -> String {
    format!(
        r#"{{
            peppy_schema: "node/v1",
            manifest: {{
                name: "robot_arm",
                tag: "{TAG}",
                depends_on: {{ pairings: [
                    {{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller", cardinality: "zero_or_one" }}
                ] }}
            }},
            interfaces: {{ topics: {{
                emits: [{{ link_id: "controller", name: "joint_states" }}],
                consumes: [{{ link_id: "controller", name: "joint_commands" }}]
            }} }},
            execution: {{
                language: "rust",
                run_cmd: ["sh", "-c", "echo $$ > {pidfile_word}; exec sleep 300"]
            }}
        }}"#,
        pidfile_word = per_instance_pidfile_word(pid_dir)
    )
}

/// The sources an observer slot's watch holds, live or not, by instance id.
fn members(rx: &watch::Receiver<ObservationState>) -> Vec<String> {
    rx.borrow()
        .members
        .iter()
        .map(|member| member.source.producer.instance_id.clone())
        .collect()
}

fn member(rx: &watch::Receiver<ObservationState>) -> peppylib::messaging::ObservedMemberState {
    let state = rx.borrow().clone();
    match state.members.as_slice() {
        [sole] => sole.clone(),
        members => panic!("expected one member, got {}", members.len()),
    }
}
