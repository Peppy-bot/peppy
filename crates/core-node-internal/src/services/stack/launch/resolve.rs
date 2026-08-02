use super::feedback::{publish_stderr, publish_stdout};
use super::{NodeKey, PlannedDeployment, ProcessLaunchContext};
use crate::services::node::pins;
use crate::services::node::resolve_node_config;
use crate::services::repo::cache as repo_cache;
use core_node_api::encoding::{
    LaunchFeedbackStep, LaunchGoal, LaunchResult, LauncherOrigin, NodeSource, PlacementSpec,
};
use daemon_config::core_node_name::CoreNodeName;
use daemon_config::format_quoted_list;
use daemon_config::launcher::{Deployment, DeploymentSource, PeppyLauncherParser, Placements};
use daemon_config::repository::PinnedItem;
use parking_lot::Mutex as StdMutex;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Why a `local:` source cannot cross a machine boundary.
///
/// Reads as one rule with [`pins::portable_pin_refusal`]: whether the tree a
/// deployment resolves to sits behind a `local:` path or a filesystem
/// repository entry, it lives on the coordinator's disk and no other machine
/// can read it.
pub(crate) fn local_source_refusal(
    spec: &daemon_config::launcher::DeploymentLocalSource,
) -> String {
    format!(
        "`local:{}` cannot be placed on another core node: the path names a tree on the \
         coordinator's filesystem. A `local:` deployment must keep all of its instances on \
         one core node; publish the node to a repo or git-backed source to split it across \
         machines.",
        spec.local.display()
    )
}

fn deployment_label(deployment: &Deployment) -> String {
    match &deployment.source {
        DeploymentSource::Local(spec) => format!("local:{}", spec.local.display()),
        DeploymentSource::Git(spec) => format!("git:{}@{}:{}", spec.repo, spec.ref_, spec.path),
        DeploymentSource::Url(spec) => format!("url:{}", spec.url),
        DeploymentSource::Repo(spec) => format!("repo:{}:{}", spec.name, spec.tag),
    }
}

/// Step 1: Parse launcher configuration from file path.
pub(super) async fn parse_launcher_config(
    ctx: &ProcessLaunchContext,
    goal: &LaunchGoal,
) -> std::result::Result<(Vec<Deployment>, PathBuf, Placements), LaunchResult> {
    publish_stdout(
        ctx,
        "Parsing launcher configuration",
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    let launch_file = match resolve_launcher_origin(ctx, &goal.launcher_origin).await {
        Ok(path) => path,
        Err(msg) => {
            publish_stderr(ctx, &msg, LaunchFeedbackStep::LauncherStep).await;
            return Err(LaunchResult::failure(&ctx.log_path, msg));
        }
    };

    if !launch_file.exists() {
        let msg = format!("launch file does not exist: {}", launch_file.display());
        publish_stderr(ctx, &msg, LaunchFeedbackStep::LauncherStep).await;
        return Err(LaunchResult::failure(&ctx.log_path, msg));
    }

    if !launch_file.is_file() {
        let msg = format!("launch file path must be a file: {}", launch_file.display());
        publish_stderr(ctx, &msg, LaunchFeedbackStep::LauncherStep).await;
        return Err(LaunchResult::failure(&ctx.log_path, msg));
    }

    let peppy_launcher = match PeppyLauncherParser::from_path(&launch_file) {
        Ok(cfg) => cfg,
        Err(e) => {
            publish_stderr(
                ctx,
                format!("Invalid launcher config: {e}"),
                LaunchFeedbackStep::LauncherStep,
            )
            .await;
            return Err(LaunchResult::failure(
                &ctx.log_path,
                format!("Invalid launcher config: {e}"),
            ));
        }
    };

    // Use the parent directory of the launch file as the nodes_directory.
    let nodes_directory = launch_file
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let placements = match resolve_placements(&peppy_launcher, goal, ctx.bound_core_node.as_str()) {
        Ok(placements) => placements,
        Err(msg) => {
            publish_stderr(ctx, &msg, LaunchFeedbackStep::LauncherStep).await;
            return Err(LaunchResult::failure(&ctx.log_path, msg));
        }
    };

    let deployments = peppy_launcher.deployments.clone();
    Ok((deployments, nodes_directory, placements))
}

/// Binds the launcher's declared core node links to the machines the caller
/// asked for, then resolves every instance to the core node it runs on.
///
/// The coordinator owns this, not the CLI, and it owns the EXPANSION as well as
/// the check. Only the daemon has the resolved document — a `Repository`
/// launcher (the shape every hub launcher uses) is read from the daemon's own
/// repo cache and the CLI may never have seen it — so `--local` arrives as an
/// intent and is expanded here against what the document actually declares.
/// Expanding it caller-side would work for a file launcher and silently do
/// nothing for a repository one.
pub(super) fn resolve_placements(
    launcher: &daemon_config::launcher::PeppyLauncher,
    goal: &LaunchGoal,
    coordinator: &str,
) -> std::result::Result<Placements, String> {
    // The daemon's own name, re-parsed rather than assumed: this is the only
    // way to obtain a `CoreNodeName`, and every name in a `Placements` is one.
    let coordinator = CoreNodeName::new(coordinator)
        .map_err(|reason| format!("this daemon's core node name is invalid: {reason}"))?;
    let declared: BTreeSet<&str> = launcher.core_nodes.iter().map(String::as_str).collect();
    let links = wire_core_node_links(&goal.placement, &declared, &coordinator)?;

    let mut by_instance = BTreeMap::new();
    for deployment in &launcher.deployments {
        for instance in &deployment.instances {
            let Some(link) = &instance.core_node else {
                continue;
            };
            // The parser refuses a `core_node` naming an undeclared link and
            // the checks above wire every declared one, so this lookup should
            // not miss. It is reported rather than asserted because the launch
            // runs in a spawned task: a panic here would take the task down
            // with no message the operator could act on, whereas a refusal
            // names the link that fell through.
            let Some(core_node) = links.get(link.as_str()) else {
                return Err(format!(
                    "instance `{}` is placed on core node link `{link}`, which this launcher \
                     does not declare. Add it to `core_nodes`, or remove the instance's \
                     `core_node` field.",
                    instance.instance_id
                ));
            };
            by_instance.insert(instance.instance_id.to_string(), core_node.clone());
        }
    }

    Ok(Placements::new(coordinator, by_instance))
}

/// Every declared core node link, wired to the machine it runs on.
///
/// Splitting this out is what lets `--local` be a real placement mode rather
/// than a caller-side shortcut: both arms end in the same "every declared link
/// has exactly one machine" post-condition, so nothing downstream has to know
/// which flag produced it.
///
/// The `--place` targets are VALIDATED here, not taken on trust. They arrived
/// on the wire, and while the CLI checks what a user types, a daemon cannot
/// assume its caller was the CLI — this is where an unchecked name would
/// otherwise enter the plan and be stamped onto every producer address.
fn wire_core_node_links<'a>(
    placement: &PlacementSpec,
    declared: &BTreeSet<&'a str>,
    coordinator: &CoreNodeName,
) -> std::result::Result<BTreeMap<&'a str, CoreNodeName>, String> {
    let places = match placement {
        // `--local` collapses the whole topology onto this machine, whatever it
        // turns out to be. A launcher that declares nothing is already entirely
        // local, so this is a no-op there rather than an error.
        PlacementSpec::Local => {
            return Ok(declared
                .iter()
                .map(|link| (*link, coordinator.clone()))
                .collect());
        }
        PlacementSpec::Places(places) => places,
    };

    if declared.is_empty() && !places.is_empty() {
        let wired = format_quoted_list(places.keys());
        return Err(format!(
            "--place was given ({wired}) but this launcher declares no `core_nodes`. Either \
             remove --place, or add a `core_nodes` list naming the machines the launcher spans."
        ));
    }

    // Every declared link must be wired exactly once, and only declared links
    // may be wired. A launcher describes a topology; refusing a partial wiring
    // is what stops half of it from silently collapsing onto the coordinator.
    let missing: Vec<&str> = declared
        .iter()
        .filter(|link| !places.contains_key(**link))
        .copied()
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "these core node links are declared but not wired: {}. Wire each one with \
             `--place <core-node-link>@<core-node>`, or run the whole launcher on this machine \
             with `--local`.",
            format_quoted_list(&missing)
        ));
    }
    let undeclared: Vec<&str> = places
        .keys()
        .map(String::as_str)
        .filter(|link| !declared.contains(link))
        .collect();
    if !undeclared.is_empty() {
        return Err(format!(
            "--place names core node links this launcher does not declare: {}. It declares: {}",
            format_quoted_list(&undeclared),
            if declared.is_empty() {
                "nothing".to_owned()
            } else {
                format_quoted_list(declared)
            }
        ));
    }

    places
        .iter()
        .map(|(link, core_node)| {
            let name = CoreNodeName::new(core_node.as_str()).map_err(|reason| {
                format!("--place wires core node link `{link}` to `{core_node}`, which {reason}")
            })?;
            // Borrow the DECLARED spelling, not the request's: the two are
            // equal here, and this keeps the map's keys tied to the document.
            let link = declared
                .get(link.as_str())
                .expect("every wired link was just checked to be declared");
            Ok((*link, name))
        })
        .collect()
}

/// Translate a `LauncherOrigin` into a concrete on-disk path.
///
/// `Fs` is a no-op; `Repository` looks up the launcher in the cache and, for git-sourced
/// entries, materializes the checkout via `ensure_checkout`. Progress lines emitted by the
/// (blocking) checkout are buffered into a `Vec` and flushed to the launch feedback topic
/// after the resolver returns: quiet for cached/Fs entries, a few lines for fresh clones.
async fn resolve_launcher_origin(
    ctx: &ProcessLaunchContext,
    origin: &LauncherOrigin,
) -> std::result::Result<PathBuf, String> {
    match origin {
        LauncherOrigin::Fs(path) => Ok(path.clone()),
        LauncherOrigin::Repository { name } => {
            let peppy_dirs = ctx.peppy_dirs.clone();
            let name_for_blocking = name.clone();
            let collected = Arc::new(StdMutex::new(Vec::<String>::new()));
            let collected_for_cb = Arc::clone(&collected);

            let result = tokio::task::spawn_blocking(move || {
                crate::services::repo::cache::resolve_repo_launcher_path(
                    &name_for_blocking,
                    &peppy_dirs,
                    &|line| {
                        collected_for_cb.lock().push(line.to_owned());
                    },
                )
            })
            .await
            .map_err(|e| format!("launcher resolver join error: {e}"))?;

            let captured: Vec<String> = std::mem::take(&mut *collected.lock());
            for line in captured {
                publish_stdout(ctx, line, LaunchFeedbackStep::LauncherStep).await;
            }
            result
        }
    }
}

/// Step 2: Resolve every deployment, once, on this daemon.
///
/// The coordinator decides which bytes the whole launch runs: a `repo:`
/// deployment resolves its transitive closure here and every resolved item
/// becomes a pin, a `git:` deployment pins the commit its ref resolved to,
/// and the contract and pairing documents every manifest names are pinned
/// beside them. Peers receive those pins and never resolve a name, so their
/// cache freshness, repository priorities and exclusions cannot change what
/// this launch runs.
///
/// Runs BEFORE the federated preflight: resolution touches no other machine
/// and tears nothing down, so every refusal here is free, and the pins must
/// exist before a reservation can carry them.
pub(super) async fn resolve_deployments(
    ctx: &ProcessLaunchContext,
    deployments: Vec<Deployment>,
    nodes_directory: &Path,
    placements: &Placements,
) -> std::result::Result<Vec<PlannedDeployment>, LaunchResult> {
    publish_stdout(
        ctx,
        format!("Resolving {} deployment(s)", deployments.len()),
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    // The node index, loaded ONCE for the whole launch. Every `repo:`
    // deployment resolves against the same one, and every materialization
    // shares it rather than copying it, so a launch pays one read of a file
    // that lists every node this machine's repositories publish. Loaded
    // lazily: a launcher with no `repo:` deployment never touches it.
    let mut node_entries: Option<Arc<Vec<repo_cache::NodeCacheEntry>>> = None;
    if deployments
        .iter()
        .any(|deployment| matches!(deployment.source, DeploymentSource::Repo(_)))
    {
        match repo_cache::load_node_cache(&ctx.peppy_dirs) {
            Ok(entries) => node_entries = Some(Arc::new(entries)),
            Err(e) => {
                let msg = format!("failed to load nodes cache: {e}");
                publish_stderr(ctx, msg.clone(), LaunchFeedbackStep::LauncherStep).await;
                return Err(LaunchResult::failure(&ctx.log_path, msg));
            }
        }
    }

    let mut planned: Vec<PlannedDeployment> = Vec::new();
    let mut planning_errors: Vec<String> = Vec::new();
    let mut planned_keys: HashSet<NodeKey> = HashSet::new();

    for deployment in deployments {
        if deployment.instances.is_empty() {
            planning_errors.push(format!(
                "deployment {} must have at least one instance",
                deployment_label(&deployment)
            ));
            continue;
        }

        // Resolution runs partly on blocking threads, so progress lines are
        // buffered through a shared Vec and flushed here, the same shape
        // `resolve_launcher_origin` uses: quiet for cached entries, a few
        // clone lines for fresh fetches, published in order either way.
        let collected = Arc::new(StdMutex::new(Vec::<String>::new()));
        let resolved = resolve_one(
            ctx,
            &deployment,
            nodes_directory,
            placements,
            node_entries.as_ref(),
            &collected,
        )
        .await;
        let captured: Vec<String> = std::mem::take(&mut *collected.lock());
        for line in captured {
            publish_stdout(ctx, line, LaunchFeedbackStep::LauncherStep).await;
        }
        let ResolvedDeployment {
            source,
            config,
            root_pin,
            closure_pins,
            pin_manifests,
        } = match resolved {
            Ok(resolved) => resolved,
            Err(err) => {
                planning_errors.push(err);
                continue;
            }
        };
        let config_sha256 = match crate::services::node::manifest_fingerprint(&config) {
            Ok(fingerprint) => fingerprint,
            Err(err) => {
                planning_errors.push(err);
                continue;
            }
        };

        let node_name = config.manifest.name.as_str().to_owned();
        let node_tag = config.manifest.tag.clone();

        let key = NodeKey::new(&node_name, &node_tag);
        if !planned_keys.insert(key.clone()) {
            planning_errors.push(format!(
                "duplicate deployment for node {} (resolved from {})",
                key.label(),
                deployment_label(&deployment)
            ));
            continue;
        }

        publish_stdout(
            ctx,
            format!(
                "Deployment {} resolved to {}:{}",
                deployment_label(&deployment),
                node_name,
                node_tag
            ),
            LaunchFeedbackStep::LauncherStep,
        )
        .await;

        planned.push(PlannedDeployment {
            deployment,
            source,
            node_name,
            node_tag,
            config,
            config_sha256,
            root_pin,
            closure_pins,
            pin_manifests,
        });
    }

    if !planning_errors.is_empty() {
        let msg = daemon_config::format_bulleted(&planning_errors);
        publish_stderr(ctx, msg.clone(), LaunchFeedbackStep::LauncherStep).await;
        return Err(LaunchResult::failure(&ctx.log_path, msg));
    }

    Ok(planned)
}

/// What one resolved deployment contributes to the plan, before its identity
/// and doc pins are folded in. Named fields rather than a tuple: four arms
/// build one of these and `PlannedDeployment` copies it field by field, so a
/// pin slot that swapped with another would otherwise do so silently.
struct ResolvedDeployment {
    /// The source its adds dispatch with.
    source: NodeSource,
    config: config::node::NodeConfig,
    /// The root pin, when the source is pinned.
    root_pin: Option<PinnedItem>,
    /// The dependency-node pins of its closure.
    closure_pins: Vec<PinnedItem>,
    /// Every manifest in that closure, from which the doc pins are minted
    /// after the graph is validated.
    pin_manifests: Vec<config::node::Manifest>,
}

/// Whether any instance of `deployment` is placed off this daemon, which is
/// what obliges every pin it resolves to to be portable.
fn has_remote_instances(deployment: &Deployment, placements: &Placements) -> bool {
    deployment
        .instances
        .iter()
        .any(|instance| placements.of(instance.instance_id.as_str()) != placements.coordinator())
}

/// Refuses any pin in `pins` that another machine could not read, for a
/// deployment with at least one instance placed elsewhere.
fn refuse_unportable_pins<'a>(
    label: &str,
    pins: impl Iterator<Item = &'a PinnedItem>,
) -> std::result::Result<(), String> {
    for pin in pins {
        if let Some(reason) = pins::portable_pin_refusal(pin) {
            return Err(format!("deployment {label}: {reason}"));
        }
    }
    Ok(())
}

async fn resolve_one(
    ctx: &ProcessLaunchContext,
    deployment: &Deployment,
    nodes_directory: &Path,
    placements: &Placements,
    node_entries: Option<&Arc<Vec<repo_cache::NodeCacheEntry>>>,
    collected: &Arc<StdMutex<Vec<String>>>,
) -> std::result::Result<ResolvedDeployment, String> {
    let label = deployment_label(deployment);
    publish_stdout(
        ctx,
        format!("Retrieving node config for {label}"),
        LaunchFeedbackStep::LauncherStep,
    )
    .await;
    let remote = has_remote_instances(deployment, placements);
    let shared_feedback = || -> crate::services::node::cache::MaterializeFeedback {
        let sink = Arc::clone(collected);
        Arc::new(move |line: &str| sink.lock().push(line.to_owned()))
    };

    match &deployment.source {
        DeploymentSource::Local(spec) => {
            if remote {
                return Err(local_source_refusal(spec));
            }
            let source = NodeSource::Fs(if spec.local.is_absolute() {
                spec.local.clone()
            } else {
                nodes_directory.join(&spec.local)
            });
            let config = resolve_node_config(source.clone(), &ctx.peppy_dirs)
                .await
                .map_err(|e| {
                    format!("failed to retrieve node config for deployment {label}: {e}")
                })?;
            let manifests = vec![config.manifest.clone()];
            Ok(ResolvedDeployment {
                source,
                config,
                root_pin: None,
                closure_pins: Vec::new(),
                pin_manifests: manifests,
            })
        }
        DeploymentSource::Repo(spec) => {
            let entries = node_entries
                .expect("a repo: deployment always resolves against a loaded node index");
            let closure = pins::resolve_pinned_closure(
                &ctx.peppy_dirs,
                entries,
                &spec.name,
                &spec.tag,
                shared_feedback(),
            )
            .await
            .map_err(|e| format!("deployment {label}: {e}"))?;
            if remote {
                refuse_unportable_pins(&label, closure.nodes.iter().map(|node| &node.pin))?;
            }
            let dep_pins = closure.dep_pins();
            let manifests = closure.manifests();
            // The closure is owned here, so the root comes out by move: its
            // config and pin are the two largest things in it.
            let mut nodes = closure.nodes;
            let root = nodes.remove(0);
            let source = pinned_source(&label, &root.pin)?;
            Ok(ResolvedDeployment {
                source,
                config: root.config,
                root_pin: Some(root.pin),
                closure_pins: dep_pins,
                pin_manifests: manifests,
            })
        }
        DeploymentSource::Git(spec) => {
            let root = pins::resolve_git_deployment(
                &ctx.peppy_dirs,
                &spec.repo,
                &spec.path,
                Some(spec.ref_.as_str()),
                shared_feedback(),
            )
            .await
            .map_err(|e| format!("deployment {label}: {e}"))?;
            if remote {
                refuse_unportable_pins(&label, std::iter::once(&root.pin))?;
            }
            let source = pinned_source(&label, &root.pin)?;
            let manifests = vec![root.config.manifest.clone()];
            Ok(ResolvedDeployment {
                source,
                config: root.config,
                root_pin: Some(root.pin),
                closure_pins: Vec::new(),
                pin_manifests: manifests,
            })
        }
        DeploymentSource::Url(spec) => {
            let source = NodeSource::Http {
                url: url::Url::parse(&spec.url)
                    .map_err(|e| format!("invalid HTTP URL `{}`: {e}", spec.url))?,
                sha256: Some(spec.sha256.clone()),
            };
            let config = resolve_node_config(source.clone(), &ctx.peppy_dirs)
                .await
                .map_err(|e| {
                    format!("failed to retrieve node config for deployment {label}: {e}")
                })?;
            let manifests = vec![config.manifest.clone()];
            Ok(ResolvedDeployment {
                source,
                config,
                root_pin: None,
                closure_pins: Vec::new(),
                pin_manifests: manifests,
            })
        }
    }
}

/// The add source a pinned deployment dispatches with. Shared by the `repo:`
/// and `git:` arms so the two cannot disagree on how a root pin reaches the
/// machine that materializes it.
fn pinned_source(label: &str, pin: &PinnedItem) -> std::result::Result<NodeSource, String> {
    Ok(NodeSource::Pinned {
        pin_json5: serde_json5::to_string(pin)
            .map_err(|e| format!("deployment {label}: could not encode its pin: {e}"))?,
    })
}

/// Mints the contract and pairing document pins of every planned deployment,
/// after the graph validation has run.
///
/// Deliberately a separate step from node resolution: the graph refusals
/// (an unimplemented binding, an uncovered pairing slot) point at the
/// launcher and the manifests, and are the more actionable ones when a
/// document is also missing from this machine's caches. Minting after them
/// keeps that precedence while still landing every doc refusal before any
/// machine is reserved or torn down.
///
/// The pins land on each deployment's `closure_pins`, beside its
/// dependency-node pins, so every add of the deployment carries them.
pub(super) async fn mint_doc_pins(
    ctx: &ProcessLaunchContext,
    planned: &mut [PlannedDeployment],
    placements: &Placements,
) -> std::result::Result<(), LaunchResult> {
    // Every deployment mints against ONE load of the contract and pairing
    // caches, on one blocking thread: both are read and parsed per load, and
    // a launch with N deployments otherwise paid N of each, sequentially, on
    // the critical path.
    let collected = Arc::new(StdMutex::new(Vec::<String>::new()));
    let sets: Vec<Vec<config::node::Manifest>> = planned
        .iter()
        .map(|item| item.pin_manifests.clone())
        .collect();
    let minted = pins::doc_pins_for_manifest_sets_async(&ctx.peppy_dirs, sets, {
        let sink = Arc::clone(&collected);
        Arc::new(move |line: &str| sink.lock().push(line.to_owned()))
    })
    .await;
    let captured: Vec<String> = std::mem::take(&mut *collected.lock());
    for line in captured {
        publish_stdout(ctx, line, LaunchFeedbackStep::LauncherStep).await;
    }

    let minted = match minted {
        Ok(minted) => minted,
        Err(reason) => {
            publish_stderr(ctx, reason.clone(), LaunchFeedbackStep::LauncherStep).await;
            return Err(LaunchResult::failure(&ctx.log_path, reason));
        }
    };

    let mut problems: Vec<String> = Vec::new();
    for (item, minted) in planned.iter_mut().zip(minted) {
        let label = deployment_label(&item.deployment);
        match minted {
            Ok(doc_pins) => {
                if has_remote_instances(&item.deployment, placements)
                    && let Err(reason) = refuse_unportable_pins(&label, doc_pins.iter())
                {
                    problems.push(reason);
                    continue;
                }
                item.closure_pins.extend(doc_pins);
            }
            Err(reason) => problems.push(format!("deployment {label}: {reason}")),
        }
    }
    if !problems.is_empty() {
        let msg = daemon_config::format_bulleted(&problems);
        publish_stderr(ctx, msg.clone(), LaunchFeedbackStep::LauncherStep).await;
        return Err(LaunchResult::failure(&ctx.log_path, msg));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use daemon_config::launcher::DeploymentInstance;
    use daemon_config::repository::{
        EntryOrigin, GitCommit, ItemName, ItemTag, ManifestFingerprint, PinKind, RepoRelativePath,
    };

    fn source(json5: &str) -> DeploymentSource {
        serde_json5::from_str(json5).expect("valid deployment source")
    }

    fn deployment(source_json5: &str, instance_ids: &[&str]) -> Deployment {
        Deployment {
            source: source(source_json5),
            instances: instance_ids
                .iter()
                .map(|id| {
                    DeploymentInstance::empty(config::runtime::Name::new(*id).expect("valid name"))
                })
                .collect(),
        }
    }

    fn placed(pairs: &[(&str, &str)]) -> Placements {
        Placements::new(
            core_node("cn-robot"),
            pairs
                .iter()
                .map(|(id, target)| ((*id).to_owned(), core_node(target)))
                .collect(),
        )
    }

    fn test_pin(origin: EntryOrigin) -> PinnedItem {
        PinnedItem {
            kind: PinKind::Node,
            name: ItemName::parse("planner").expect("valid name"),
            tag: ItemTag::parse("v1").expect("valid tag"),
            sha256: ManifestFingerprint::parse(&"a".repeat(64)).expect("valid sha"),
            origin,
        }
    }

    /// A deployment with every instance on the coordinator obliges its pins
    /// to nothing; one with any instance elsewhere obliges every pin it
    /// resolves to to be readable off this machine.
    #[test]
    fn only_off_coordinator_instances_oblige_portable_pins() {
        let local_only = deployment(r#"{ name: "planner", tag: "v1" }"#, &["planner_1"]);
        let placements = placed(&[("cam_cloud", "cn-atlas")]);
        assert!(!has_remote_instances(&local_only, &placements));

        let split = deployment(
            r#"{ name: "planner", tag: "v1" }"#,
            &["planner_1", "cam_cloud"],
        );
        assert!(has_remote_instances(&split, &placements));
    }

    /// The refusal the portability rule produces: a filesystem repository
    /// entry behind an off-coordinator placement names a tree only this
    /// machine can read, exactly like a `local:` source, and the message
    /// says what still works.
    #[test]
    fn a_filesystem_backed_pin_is_refused_for_a_remote_placement() {
        let fs_pin = test_pin(EntryOrigin::Fs {
            path: "/repo/planner/peppy.json5".into(),
        });
        let error = refuse_unportable_pins("repo:planner:v1", std::iter::once(&fs_pin))
            .expect_err("a filesystem origin cannot cross machines");
        assert!(error.contains("coordinator's filesystem"), "got: {error}");
        assert!(error.contains("git repository"), "got: {error}");

        let git_pin = test_pin(EntryOrigin::Git {
            repo_url: "https://example.com/hub".to_owned(),
            repo_ref: Some("main".to_owned()),
            commit: GitCommit::parse(&"b".repeat(40)).expect("valid commit"),
            path: RepoRelativePath::parse("planner/peppy.json5").expect("valid path"),
        });
        assert!(refuse_unportable_pins("repo:planner:v1", std::iter::once(&git_pin)).is_ok());
    }

    /// A path names a tree on the coordinator's disk, so a peer resolving it
    /// would read a different tree or nothing at all.
    #[test]
    fn the_local_source_refusal_names_the_tree_and_the_way_out() {
        let DeploymentSource::Local(spec) = source(r#"{ local: "./nodes/planner" }"#) else {
            panic!("expected a local source");
        };
        let error = local_source_refusal(&spec);
        assert!(error.contains("names a tree on"), "got: {error}");
        assert!(error.contains("repo or git-backed source"), "got: {error}");
    }

    // --- Placement: binding a launcher's declared links to real machines ---

    fn declared<'a>(links: &[&'a str]) -> BTreeSet<&'a str> {
        links.iter().copied().collect()
    }

    fn core_node(name: &str) -> CoreNodeName {
        CoreNodeName::new(name).expect("valid test core node name")
    }

    fn places(pairs: &[(&str, &str)]) -> PlacementSpec {
        PlacementSpec::Places(
            pairs
                .iter()
                .map(|(link, target)| ((*link).to_owned(), (*target).to_owned()))
                .collect(),
        )
    }

    /// The case the CLI cannot serve: `--local` against a launcher whose
    /// document only the coordinator has read. Expanding here is what makes
    /// `peppy stack launch --local <name>` work at all — a repository launcher
    /// is the shape every hub launcher uses, and the CLI never sees it.
    #[test]
    fn local_wires_every_declared_link_to_the_coordinator() {
        let wired = wire_core_node_links(
            &PlacementSpec::Local,
            &declared(&["robot_onboard", "cloud_inference"]),
            &core_node("cn-robot-7"),
        )
        .expect("--local always wires");

        assert_eq!(wired.len(), 2);
        assert!(wired.values().all(|target| target.as_str() == "cn-robot-7"));
    }

    /// A launcher that declares no links is already entirely local, so
    /// `--local` is a no-op there rather than an error.
    #[test]
    fn local_on_a_single_machine_launcher_wires_nothing_and_is_accepted() {
        let wired = wire_core_node_links(
            &PlacementSpec::Local,
            &declared(&[]),
            &core_node("cn-robot-7"),
        )
        .expect("a single-machine launcher is trivially local");
        assert!(wired.is_empty());
    }

    #[test]
    fn place_wires_each_declared_link_to_its_named_machine() {
        let placement = places(&[
            ("robot_onboard", "cn-robot-7"),
            ("cloud_inference", "cn-atlas-h100"),
        ]);
        let wired = wire_core_node_links(
            &placement,
            &declared(&["robot_onboard", "cloud_inference"]),
            &core_node("cn-robot-7"),
        )
        .expect("a full wiring");
        assert_eq!(wired["cloud_inference"].as_str(), "cn-atlas-h100");
        assert_eq!(wired["robot_onboard"].as_str(), "cn-robot-7");
    }

    /// A launcher describes a topology; refusing a partial wiring is what
    /// stops half of it from silently collapsing onto the coordinator.
    #[test]
    fn a_declared_but_unwired_link_is_refused_and_names_local_as_the_way_out() {
        let placement = places(&[("robot_onboard", "cn-robot-7")]);
        let error = wire_core_node_links(
            &placement,
            &declared(&["robot_onboard", "cloud_inference"]),
            &core_node("cn-robot-7"),
        )
        .expect_err("a partial wiring must be refused");
        assert!(error.contains("cloud_inference"), "got: {error}");
        assert!(error.contains("--local"), "got: {error}");
    }

    /// Every declared link is wired here, so the only thing left to object to
    /// is the extra one — which is what isolates this rule from the
    /// unwired-link check that runs before it.
    #[test]
    fn placing_a_link_the_launcher_does_not_declare_is_refused() {
        let placement = places(&[
            ("robot_onboard", "cn-robot-7"),
            ("typo_onboard", "cn-atlas-h100"),
        ]);
        let error = wire_core_node_links(
            &placement,
            &declared(&["robot_onboard"]),
            &core_node("cn-robot-7"),
        )
        .expect_err("only declared links may be wired");
        assert!(error.contains("does not declare"), "got: {error}");
        assert!(error.contains("typo_onboard"), "got: {error}");
    }

    #[test]
    fn placing_against_a_launcher_that_declares_nothing_is_refused() {
        let placement = places(&[("robot_onboard", "cn-robot-7")]);
        let error = wire_core_node_links(&placement, &declared(&[]), &core_node("cn-robot-7"))
            .expect_err("there is nothing to wire");
        assert!(error.contains("declares no `core_nodes`"), "got: {error}");
    }

    /// No flag at all is not the same as `--local`: it leaves a declared link
    /// unwired, which is refused. The distinction is why the intent travels as
    /// an enum rather than as "an empty map means local".
    #[test]
    fn no_placement_flag_is_not_treated_as_local() {
        let error = wire_core_node_links(
            &PlacementSpec::default(),
            &declared(&["robot_onboard"]),
            &core_node("cn-robot-7"),
        )
        .expect_err("an unwired declared link must be refused");
        assert!(error.contains("declared but not wired"), "got: {error}");
    }
}
