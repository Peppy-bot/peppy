//! The host paths a machine's containers bind: which sources each machine of
//! a plan needs, and making them usable there.
//!
//! Every daemon that runs a slice of a launch does this once, while its stack
//! is empty: the coordinator as a launch step, a participant when it is told to
//! replace its slice. The reason it cannot be left to the instance starts that
//! need the paths is [`Apptainer::ensure_host_mounts`]: registering a host path
//! the container VM has not seen restarts that VM, and a restart takes every
//! container already running in it. Doing the whole machine's registration
//! before the first instance starts is what keeps that restart free.
//!
//! On Linux there is no VM and `ensure_host_mounts` is a no-op, so what remains
//! is the auto-create of missing sources, which every machine owes its own
//! instances.
//!
//! Sources may arrive as raw `~/...` tokens: the coordinator resolves the
//! plan's mount paths but deliberately leaves `~` alone, because a peer's home
//! is not the coordinator's. This is the place they become this machine's
//! absolute paths — before anything is created or registered.

use super::action::StackChangeContext;
use super::launch::feedback::{publish_stderr, publish_stdout};
use super::launch::{NodeKey, PlannedDeployment};
use crate::services::node::resolve_mount_path_parameters;
use config::apply_parameter_defaults;
use containers::{Apptainer, is_host_provided_mount_source};
use core_node_api::encoding::LaunchFeedbackStep;
use daemon_config::launcher::Placements;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Makes this machine's bind sources usable, and reports the ones it created.
///
/// Creation comes first and registration second, on purpose: Lima drops a
/// mount whose host path does not exist, so registering an absent source would
/// buy a VM restart now and another one later when the path appears.
///
/// The returned paths are the ones that did not exist. Each is an operator
/// warning its caller must surface in whatever stream it owns, because an
/// auto-created source is also what a bind meant to name an existing file looks
/// like when the name is misspelled. A `~` source is reported (and created,
/// registered) in its expanded form: the expanded path is what actually lands
/// on this machine's disk.
pub(crate) async fn prepare_container_mounts(
    mount_sources: &[String],
) -> std::result::Result<Vec<String>, String> {
    prepare_mounts(mount_sources, true).await
}

/// Adds bind sources to the VM while it keeps hosting the stack's containers.
pub(crate) async fn prepare_additional_container_mounts(
    mount_sources: &[String],
) -> std::result::Result<Vec<String>, String> {
    prepare_mounts(mount_sources, false).await
}

async fn prepare_mounts(
    mount_sources: &[String],
    allow_vm_restart: bool,
) -> std::result::Result<Vec<String>, String> {
    // Expanding here and at instance start through the one shared helper keeps
    // both sides byte-identical: `host_mounts_need_restart` at start compares
    // against what was registered now, and a `~` source registered in one form
    // but checked in another would demand a spurious VM restart.
    let mount_sources = mount_sources
        .iter()
        .map(|src| containers::expand_home_in_mount_spec(src))
        .collect::<std::result::Result<Vec<String>, String>>()?;

    let mut auto_created = Vec::new();
    for src in &mount_sources {
        if containers::ensure_bind_source(Path::new(src)).map_err(|e| e.to_string())? {
            auto_created.push(src.clone());
        }
    }

    let lima_mount_sources = external_lima_mount_sources(&mount_sources);
    if lima_mount_sources.is_empty() {
        return Ok(auto_created);
    }

    // A first-time mount registration restarts the Lima VM, so this runs on the
    // blocking pool rather than holding the async worker for the restart.
    tokio::task::spawn_blocking(move || {
        let mut apptainer =
            Apptainer::new().map_err(|e| format!("Failed to initialize Apptainer: {e}"))?;
        let refs = lima_mount_sources
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        if !allow_vm_restart
            && apptainer
                .host_mounts_need_restart(&refs)
                .map_err(|e| e.to_string())?
        {
            return Err(format!(
                "bind sources {} need a container VM restart, which would stop the containers \
                 already running; use paths under your home directory, or prepare the mounts \
                 before launching the stack",
                daemon_config::format_quoted_list(&refs)
            ));
        }
        apptainer
            .ensure_host_mounts(&refs)
            .map_err(|e| format!("Failed to prepare container host mounts: {e}"))
    })
    .await
    .map_err(|e| format!("Failed to prepare container host mounts: {e}"))
    .and_then(|result| result)?;

    Ok(auto_created)
}

/// The subset of `mount_sources` a Lima guest cannot already see, absolute.
///
/// Only macOS runs containers in a VM. Home-relative paths arrive through
/// Lima's default `~` mount and host-provided trees are the guest's own, so
/// neither is ours to register; what is left is the paths that need an explicit
/// mount, which is what costs a VM restart.
///
/// Absolute because a registration ends up as a `location:` in the Lima config,
/// which the VM resolves with no notion of the daemon's working directory: a
/// relative source has to be anchored the same way the create above anchored
/// it, or the guest would mount something else, or nothing.
fn external_lima_mount_sources(mount_sources: &[String]) -> Vec<String> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }

    let home = std::env::var_os("HOME").map(PathBuf::from);
    mount_sources
        .iter()
        .filter_map(|src| {
            let src_path = absolute_mount_source(src);
            let guest_already_sees = is_host_provided_mount_source(&src_path)
                || home
                    .as_ref()
                    .is_some_and(|home_path| src_path.starts_with(home_path));
            (!guest_already_sees).then(|| src_path.to_string_lossy().into_owned())
        })
        .collect()
}

fn absolute_mount_source(src: &str) -> PathBuf {
    let path = Path::new(src);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

/// Which stack this machine's bind sources are prepared for.
#[derive(Clone, Copy)]
pub(in crate::services::stack) enum LocalMounts {
    /// The whole stack a launch is starting, which holds no container yet, so
    /// registering a source may restart the container VM.
    WholeStack,
    /// The instances a join adds, alongside the containers the VM already
    /// hosts for the running stack.
    AddedToRunningStack,
}

/// Prepares this machine's bind sources and reports each one it had to
/// create. Each machine prepares the sources of its own instances: a
/// participant was handed its share when it was told to replace its slice.
pub(in crate::services::stack) async fn prepare_local_container_mounts(
    ctx: &StackChangeContext,
    has_container_nodes: bool,
    mut mount_sources: Vec<String>,
    mounts: LocalMounts,
) -> std::result::Result<(), String> {
    // The peppy data root hosts the container build working dirs (`tmp/`),
    // built images (`built_nodes/`), and instance dirs. When it sits outside
    // `$HOME` (dev roots at `$TMPDIR/.peppy`) the Lima guest cannot see it,
    // so register it here whenever the stack has container nodes. It always
    // exists, so it never reaches the auto-create warning path, and
    // `external_lima_mount_sources` filters it out on Linux and for
    // home-relative roots (prod).
    if has_container_nodes {
        let root = ctx
            .peppy_dirs
            .root()
            .to_str()
            .ok_or_else(|| "peppy root path is not valid UTF-8".to_string())?;
        mount_sources.push(root.to_owned());
    }

    if mount_sources.is_empty() {
        return Ok(());
    }

    publish_stdout(
        ctx,
        "Preparing container host mounts",
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    let auto_created = match mounts {
        LocalMounts::WholeStack => prepare_container_mounts(&mount_sources).await?,
        LocalMounts::AddedToRunningStack => {
            prepare_additional_container_mounts(&mount_sources).await?
        }
    };
    for src in auto_created {
        publish_stderr(
            ctx,
            containers::auto_created_warning(&src),
            LaunchFeedbackStep::LauncherStep,
        )
        .await;
    }
    Ok(())
}

/// Whether `core_node` hosts an instance of a container node among `items`.
pub(in crate::services::stack) fn hosts_container_nodes<'a>(
    items: impl IntoIterator<Item = &'a PlannedDeployment>,
    placements: &Placements,
    core_node: &str,
) -> bool {
    items
        .into_iter()
        .filter(|item| item.config.execution.container.is_some())
        .any(|item| {
            item.deployment
                .instances
                .iter()
                .any(|instance| placements.of(instance.instance_id.as_str()) == core_node)
        })
}

/// The host paths each machine's container instances bind, keyed by core node.
///
/// Resolved here, on the coordinator, because only the coordinator holds the
/// whole plan: a mount path may name an instance parameter, and a machine is
/// handed one instance at a time. Machines with nothing to bind are absent
/// rather than present-and-empty, so a caller iterating this map is iterating
/// the machines that have work to do.
///
/// Called before the launch turns destructive, which is what makes an
/// unresolvable mount path (a parameter with no value) cost nobody their stack.
pub(super) fn container_mount_sources_by_machine(
    planned: &[PlannedDeployment],
    placements: &Placements,
) -> std::result::Result<HashMap<String, Vec<String>>, String> {
    let mut by_machine: HashMap<String, Vec<String>> = HashMap::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    for item in planned {
        let Some(container) = item.config.execution.container.as_ref() else {
            continue;
        };
        let raw_mount_paths = container.mount_paths.as_deref().unwrap_or_default();
        if raw_mount_paths.is_empty() {
            continue;
        }
        let label = NodeKey::new(&item.node_name, &item.node_tag).label();

        for instance in &item.deployment.instances {
            let machine = placements.of(instance.instance_id.as_str());
            let mut arguments = instance.arguments.clone();
            let missing =
                apply_parameter_defaults(&mut arguments, &item.config.execution.parameters);
            if !missing.is_empty() {
                return Err(format!(
                    "failed to prepare container mounts for {label} instance {}: Missing required parameters: {}",
                    instance.instance_id,
                    missing.join(", ")
                ));
            }

            let resolved_mount_paths = resolve_mount_path_parameters(raw_mount_paths, &arguments)
                .map_err(|msg| {
                format!(
                    "failed to prepare container mounts for {label} instance {}: {msg}",
                    instance.instance_id,
                )
            })?;
            for mount in resolved_mount_paths {
                let src = containers::mount_spec_source(&mount).to_string();
                if seen.insert((machine.to_owned(), src.clone())) {
                    by_machine.entry(machine.to_owned()).or_default().push(src);
                }
            }
        }
    }

    Ok(by_machine)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::stack::fixtures::{
        DEFAULTED_OUTPUT_DIR, REQUIRED_OUTPUT_DIR, placements_with, planned_container_deployment,
    };
    use config::AnyType;
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn prepare_reports_only_the_sources_it_created() {
        let root = tempfile::tempdir().expect("tempdir");
        let existing = root.path().join("already_there");
        std::fs::create_dir(&existing).expect("mkdir");
        let missing = root.path().join("scratch").join("output");

        let auto_created = prepare_container_mounts(&[
            existing.to_string_lossy().into_owned(),
            missing.to_string_lossy().into_owned(),
        ])
        .await
        .expect("both sources are preparable");

        assert_eq!(
            auto_created,
            vec![missing.to_string_lossy().into_owned()],
            "only the missing source is an operator warning"
        );
        assert!(
            missing.is_dir(),
            "the missing source must have been created"
        );
    }

    /// The whole preparation fails rather than half-registering a machine: a
    /// source that cannot be created is a bind that cannot work, and the launch
    /// that needs it is better stopped here than at the instance start.
    #[cfg(unix)]
    #[tokio::test]
    async fn prepare_fails_when_a_source_cannot_be_created() {
        let root = tempfile::tempdir().expect("tempdir");
        let target = root.path().join("bind_source");
        std::os::unix::fs::symlink(root.path().join("no_such_target"), &target)
            .expect("plant a dangling symlink");

        let error = prepare_container_mounts(&[target.to_string_lossy().into_owned()])
            .await
            .expect_err("an uncreatable source must fail");
        assert!(
            error.contains(target.to_string_lossy().as_ref()),
            "the failure must name the offending path, got: {error}"
        );
    }

    /// The regression this module's expansion exists for: a `~` source arriving
    /// from the coordinator must be expanded to this machine's home BEFORE the
    /// auto-create, not `mkdir -p`'d as a literal relative path. The home
    /// already exists, so nothing is created and nothing is reported — and in
    /// particular no stray `~/` directory appears under the process cwd, which
    /// is exactly what the unexpanded token used to produce.
    #[tokio::test]
    async fn prepare_expands_home_relative_sources() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        assert!(home.exists(), "the test needs a real home to expand into");

        let auto_created = prepare_container_mounts(&["~".to_string()])
            .await
            .expect("a home-relative source must be preparable");
        assert!(
            auto_created.is_empty(),
            "the home exists, so nothing may be auto-created, got: {auto_created:?}"
        );
        assert!(
            !Path::new("~").exists(),
            "a literal ./~ directory must never be created"
        );
    }

    /// A host-provided source must never be handed to Lima as an extra mount.
    /// Registering `/run/user` would mount the macOS side (which does not even
    /// exist) over the guest's own runtime tmpfs, and would restart the VM to
    /// do it. The guest resolves these paths itself.
    ///
    /// macOS-gated like its companions below: off macOS
    /// `external_lima_mount_sources` returns empty before consulting the
    /// filter at all, so an ungated assertion would hold with the filter
    /// deleted and prove nothing. The predicate itself is platform-independent
    /// and covered in `containers::mount_source`.
    #[test]
    #[cfg(target_os = "macos")]
    fn host_provided_sources_are_not_forwarded_to_lima() {
        let forwarded = external_lima_mount_sources(&[
            "/run/user".to_string(),
            "/dev/ttyUSB0".to_string(),
            "/proc/self".to_string(),
            "/sys/class".to_string(),
        ]);
        assert!(
            forwarded.is_empty(),
            "host-provided trees must stay out of the Lima mount list, got: {forwarded:?}"
        );
    }

    /// Lima mounts `$HOME` itself, so a path under it is already visible to the
    /// guest and registering it would buy a VM restart for nothing.
    #[test]
    #[cfg(target_os = "macos")]
    fn home_relative_sources_are_not_forwarded_to_lima() {
        let home = std::env::var("HOME").expect("HOME is set on a macOS test host");
        let forwarded = external_lima_mount_sources(&[format!("{home}/.peppy/built_nodes")]);
        assert!(
            forwarded.is_empty(),
            "a path Lima already mounts must not be registered again, got: {forwarded:?}"
        );
    }

    /// The complement of the tests above: the filter is a carve-out, not a
    /// blanket opt-out. An ordinary path outside `$HOME` still has to reach
    /// Lima or the guest could not see it. Only meaningful on macOS, where
    /// `external_lima_mount_sources` does its work.
    #[test]
    #[cfg(target_os = "macos")]
    fn ordinary_external_sources_are_still_forwarded_to_lima() {
        let forwarded = external_lima_mount_sources(&["/opt/robot_assets".to_string()]);
        assert_eq!(
            forwarded,
            vec!["/opt/robot_assets".to_string()],
            "a non-home path the guest cannot otherwise see must be registered",
        );
    }

    /// A relative source is decided against its absolute form, so it must be
    /// registered in that form too: a `location:` in the Lima config is
    /// resolved by the VM, which has no working directory to relate it to.
    ///
    /// Which branch applies depends on where the test binary runs: a checkout
    /// under `$HOME` makes the path one Lima already mounts, and dropping it is
    /// the same anchoring decision seen from the other side.
    #[test]
    #[cfg(target_os = "macos")]
    fn relative_sources_are_forwarded_to_lima_absolute() {
        let cwd = std::env::current_dir().expect("a working directory");
        let home = std::env::var("HOME").expect("HOME is set on a macOS test host");
        let forwarded = external_lima_mount_sources(&["robot_assets".to_string()]);

        if cwd.starts_with(&home) {
            assert!(
                forwarded.is_empty(),
                "a relative source under $HOME resolves into Lima's own mount, got: {forwarded:?}"
            );
        } else {
            assert_eq!(
                forwarded,
                vec![cwd.join("robot_assets").to_string_lossy().into_owned()],
                "a relative source must reach Lima anchored, not verbatim",
            );
        }
    }

    /// Each machine gets its own instances' sources and nobody else's. This is
    /// what a participant is handed to prepare, so a source landing on the
    /// wrong machine would make a directory there and still leave the binding
    /// machine without one.
    #[test]
    fn mount_sources_are_grouped_by_the_machine_that_binds_them() {
        let planned = vec![planned_container_deployment(
            "recorder",
            DEFAULTED_OUTPUT_DIR,
            &["/data/episodes:/episodes:rw"],
            &[
                ("robot_inst", None, BTreeMap::new()),
                ("cloud_inst", Some("cn-cloud"), BTreeMap::new()),
            ],
        )];
        let placements = placements_with("cn-robot", &[("cloud_inst", "cn-cloud")]);

        let by_machine = container_mount_sources_by_machine(&planned, &placements)
            .expect("every mount path resolves");

        assert_eq!(
            by_machine.get("cn-robot").map(Vec::as_slice),
            Some(["/data/episodes".to_owned()].as_slice())
        );
        assert_eq!(
            by_machine.get("cn-cloud").map(Vec::as_slice),
            Some(["/data/episodes".to_owned()].as_slice())
        );
    }

    /// A machine with nothing to bind is absent, not present-and-empty: the
    /// caller iterates this map to decide who has preparation to do.
    #[test]
    fn a_machine_running_no_container_bind_is_absent_from_the_grouping() {
        let planned = vec![
            planned_container_deployment(
                "recorder",
                DEFAULTED_OUTPUT_DIR,
                &["/data/episodes"],
                &[("cloud_inst", Some("cn-cloud"), BTreeMap::new())],
            ),
            planned_container_deployment(
                "camera",
                DEFAULTED_OUTPUT_DIR,
                &[],
                &[("robot_inst", None, BTreeMap::new())],
            ),
        ];
        let placements = placements_with("cn-robot", &[("cloud_inst", "cn-cloud")]);

        let by_machine = container_mount_sources_by_machine(&planned, &placements)
            .expect("every mount path resolves");

        assert_eq!(by_machine.len(), 1);
        assert!(by_machine.contains_key("cn-cloud"));
    }

    /// Two instances of one node on one machine binding the same path is one
    /// source, and the same path on two machines is one source each: the map
    /// dedupes per machine, not globally.
    #[test]
    fn a_repeated_source_is_listed_once_per_machine() {
        let mut arguments = BTreeMap::new();
        arguments.insert(
            "output_dir".to_owned(),
            AnyType::String("/data/shared".to_owned()),
        );
        let planned = vec![planned_container_deployment(
            "recorder",
            DEFAULTED_OUTPUT_DIR,
            &["${parameters:output_dir}:/out:rw"],
            &[
                ("first_inst", None, arguments.clone()),
                ("second_inst", None, arguments),
            ],
        )];

        let by_machine =
            container_mount_sources_by_machine(&planned, &placements_with("cn-robot", &[]))
                .expect("every mount path resolves");

        assert_eq!(
            by_machine.get("cn-robot").map(Vec::as_slice),
            Some(["/data/shared".to_owned()].as_slice())
        );
    }

    /// A parameter the instance never supplies falls back to the node's
    /// default, so the machine that runs it still knows what to prepare.
    #[test]
    fn a_defaulted_mount_parameter_resolves_to_the_nodes_default() {
        let planned = vec![planned_container_deployment(
            "recorder",
            DEFAULTED_OUTPUT_DIR,
            &["${parameters:output_dir}:/out:rw"],
            &[("robot_inst", None, BTreeMap::new())],
        )];

        let by_machine =
            container_mount_sources_by_machine(&planned, &placements_with("cn-robot", &[]))
                .expect("the parameter default resolves");

        assert_eq!(
            by_machine.get("cn-robot").map(Vec::as_slice),
            Some(["/var/lib/peppy_default".to_owned()].as_slice())
        );
    }

    /// An unresolvable mount path names the instance it belongs to. This runs
    /// before the launch turns destructive, so it is the operator's whole
    /// description of what went wrong.
    #[test]
    fn an_unresolvable_mount_path_names_its_instance() {
        let planned = vec![planned_container_deployment(
            "recorder",
            REQUIRED_OUTPUT_DIR,
            &["${parameters:output_dir}:/out:rw"],
            &[("robot_inst", None, BTreeMap::new())],
        )];

        let error = container_mount_sources_by_machine(&planned, &placements_with("cn-robot", &[]))
            .expect_err("an unknown parameter cannot resolve");
        assert!(error.contains("recorder:v1"), "got: {error}");
        assert!(error.contains("robot_inst"), "got: {error}");
    }

    /// One deployment of plain instances, each optionally placed. Enough for
    /// the placement-derived checks, which read instance ids and nothing else.

    #[test]
    fn launch_mount_preflight_resolves_parameterized_sources() {
        let mut video = BTreeMap::new();
        video.insert(
            "output_dir".to_string(),
            AnyType::String("/tmp/video_reconstruction".to_string()),
        );
        let mut arguments = BTreeMap::new();
        arguments.insert("video".to_string(), AnyType::Object(video));

        let resolved = resolve_mount_path_parameters(
            &["${parameters:video.output_dir}:/frames:rw".to_string()],
            &arguments,
        )
        .expect("parameterized mount should resolve");

        assert_eq!(resolved, vec!["/tmp/video_reconstruction:/frames:rw"]);
        assert_eq!(
            containers::mount_spec_source(&resolved[0]),
            "/tmp/video_reconstruction"
        );
    }

    /// An instance's `env_vars` are added to the forwarded caller environment
    /// and win on a shared key, leaving exactly one entry per key so the spawn

    #[test]
    fn launch_mount_preflight_rejects_non_string_parameter_sources() {
        let mut arguments = BTreeMap::new();
        arguments.insert("frame_rate".to_string(), AnyType::UInt(30));

        let err = resolve_mount_path_parameters(
            &["${parameters:frame_rate}:/frames:rw".to_string()],
            &arguments,
        )
        .expect_err("non-string mount parameter should be rejected");

        assert!(err.contains("must be a string"));
    }
}
