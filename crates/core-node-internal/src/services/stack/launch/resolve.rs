use super::feedback::{publish_error, publish_stdout, spawn_feedback_forwarder};
use super::{NodeKey, PlannedDeployment};
use crate::services::node::pins;
use crate::services::node::{FeedbackLine, Report, unlogged_step_sink};
use crate::services::repo::cache as repo_cache;
use crate::services::stack::action::StackChangeContext;
use config::runtime::{CopyName, CoreNodeName, CoreNodeTarget};
use core_node_api::encoding::{LaunchFeedbackStep, LaunchGoal, LauncherOrigin, PlacementSpec};
use daemon_config::format_quoted_list;
use daemon_config::launcher::{Deployment, DeploymentSource, PeppyLauncherParser, Placements};
use daemon_config::repository::{DeploymentRoot, PinnedItem};
use parking_lot::Mutex;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

/// The launcher as the coordinator read it: the document its copies join
/// onto, the composition the launch selection produced, where each instance
/// runs, and each copy's machine.
pub(in crate::services::stack) struct ParsedLaunch {
    pub(in crate::services::stack) prepared: daemon_config::launcher::PreparedLauncher,
    pub(in crate::services::stack) composed: daemon_config::launcher::ComposedLaunch,
    pub(in crate::services::stack) placements: LaunchPlacements,
}

/// Where a launch runs: every instance, and each copy's machine, the
/// machine its name places, which runs its base.
#[derive(Debug)]
pub(in crate::services::stack) struct LaunchPlacements {
    pub(in crate::services::stack) placements: Placements,
    pub(in crate::services::stack) copy_core_nodes: BTreeMap<CopyName, CoreNodeName>,
}

/// Parses the launcher configuration from its file path, resolving the
/// `--with` selection the caller asked for when the launcher declares
/// component axes.
///
/// The selection words travel on the goal verbatim and are resolved HERE,
/// for the same reason placement is: only the coordinator holds a
/// repository launcher's document, so only it knows which axes exist. A
/// caller that resolved selections itself could do so for a filesystem
/// launcher and not for a repository one, and the two origins would drift
/// into meaning different things.
///
/// Every refusal is published as the launch's error before it is returned.
pub(in crate::services::stack) async fn parse_launcher_config(
    ctx: &StackChangeContext,
    goal: &LaunchGoal,
) -> std::result::Result<ParsedLaunch, String> {
    publish_stdout(
        ctx,
        "Parsing launcher configuration",
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    let parsed = read_launcher_config(ctx, goal).await;
    if let Err(reason) = &parsed {
        publish_error(ctx, reason, LaunchFeedbackStep::LauncherStep).await;
    }
    parsed
}

/// The steps of [`parse_launcher_config`], which publishes the reason of
/// whichever step refuses.
async fn read_launcher_config(
    ctx: &StackChangeContext,
    goal: &LaunchGoal,
) -> std::result::Result<ParsedLaunch, String> {
    let launch_file = resolve_launcher_origin(ctx, &goal.launcher_origin).await?;

    if !launch_file.exists() {
        return Err(format!(
            "launch file does not exist: {}",
            launch_file.display()
        ));
    }

    if !launch_file.is_file() {
        return Err(format!(
            "launch file path must be a file: {}",
            launch_file.display()
        ));
    }

    let parsed = PeppyLauncherParser::from_path(&launch_file)
        .map_err(|e| format!("Invalid launcher config: {e}"))?;

    let prepared = daemon_config::launcher::PreparedLauncher::load(&parsed, &launch_file)
        .map_err(|e| e.to_string())?;
    // Not a parse failure: the document was already read fine. The
    // composition errors themselves name the axis and its options, so the
    // prefix only says which step refused.
    let composed = prepared
        .launch(goal.selections(), goal.joins())
        .map_err(|e| format!("Cannot resolve the launcher's components: {e}"))?;
    if !parsed.components.is_empty() {
        // The full resolution is echoed before anything runs, so the
        // operator sees which stack this launch is of, and which copies,
        // while there is still time to interrupt it.
        for line in composed.report.selection_lines() {
            publish_stdout(ctx, line, LaunchFeedbackStep::LauncherStep).await;
        }
    }

    let placements = resolve_placements(&composed, goal, ctx.bound_core_node.as_str())?;

    Ok(ParsedLaunch {
        prepared,
        composed,
        placements,
    })
}

/// Binds the launcher's declared core node links to the machines the caller
/// asked for, then resolves every instance to the core node it runs on.
///
/// Each copy runs where it is named: a `--join` copy on the machine its
/// reference names, a copy the file lists where `--place NAME@CORE_NODE`
/// puts it, and either on the coordinator when nothing names a machine. Its
/// name is the placement link of its machine, which runs its base. Then each
/// instance of a part a word places runs on the word's machine. `self`
/// travels as the coordinator, which every target resolves to here.
///
/// The coordinator owns this, not the CLI, and it owns the EXPANSION as well as
/// the check. Only the daemon has the resolved document: a `Repository`
/// launcher (the shape every hub launcher uses) is read from the daemon's own
/// repo cache, and the CLI may never have seen it. So `--local` arrives as an
/// intent and is expanded here against what the document actually declares.
/// Expanding it caller-side would work for a file launcher and silently do
/// nothing for a repository one.
pub(in crate::services::stack) fn resolve_placements(
    composed: &daemon_config::launcher::ComposedLaunch,
    goal: &LaunchGoal,
    coordinator: &str,
) -> std::result::Result<LaunchPlacements, String> {
    // The daemon's own name, re-parsed rather than assumed: this is the only
    // way to obtain a `CoreNodeName`, and every name in a `Placements` is one.
    let coordinator = CoreNodeName::new(coordinator)
        .map_err(|reason| format!("this daemon's core node name is invalid: {reason}"))?;
    let launcher = &composed.launcher;
    let declared: BTreeSet<&str> = launcher.core_nodes.iter().map(String::as_str).collect();
    let copy_core_nodes: BTreeMap<CopyName, CoreNodeName> = composed
        .copies()
        .iter()
        .map(|copy| {
            let named = goal
                .joins()
                .iter()
                .find(|join| join.name == copy.name)
                .map(|join| &join.core_node)
                .or_else(|| match goal.placement() {
                    PlacementSpec::Places(places) => places.get(copy.name.as_name()),
                    PlacementSpec::Local => None,
                })
                .unwrap_or(&CoreNodeTarget::Coordinator);
            (copy.name.clone(), named.resolve(&coordinator))
        })
        .collect();
    let links = wire_core_node_links(goal.placement(), &declared, &copy_core_nodes, &coordinator)?;

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
    for copy in composed.copies() {
        for (axis, target) in composed.part_machines_of(&copy.name) {
            let machine = target.resolve(&coordinator);
            for instance in copy.instances_of_own_axis(axis.as_str()) {
                by_instance.insert(instance.instance_id.to_string(), machine.clone());
            }
        }
    }

    Ok(LaunchPlacements {
        placements: Placements::new(coordinator, by_instance),
        copy_core_nodes,
    })
}

/// Every declared core node link, wired to the machine it runs on: each
/// copy's name to the copy's machine, and every other link to the machine
/// its `--place` wiring names, or to the coordinator under `--local`.
///
/// Splitting this out is what lets `--local` be a real placement mode rather
/// than a caller-side shortcut: both arms end in the same "every declared link
/// has exactly one machine" post-condition, so nothing downstream has to know
/// which flag produced it.
fn wire_core_node_links<'a>(
    placement: &PlacementSpec,
    declared: &BTreeSet<&'a str>,
    copies: &BTreeMap<CopyName, CoreNodeName>,
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

    let wired: BTreeMap<&str, CoreNodeName> = places
        .iter()
        .map(|(link, target)| (link.as_str(), target.resolve(coordinator)))
        .chain(
            copies
                .iter()
                .map(|(copy, machine)| (copy.as_str(), machine.clone())),
        )
        .collect();

    // Every declared link must be wired exactly once, and only declared links
    // may be wired. A launcher describes a topology; refusing a partial wiring
    // is what stops half of it from silently collapsing onto the coordinator.
    let missing: Vec<&str> = declared
        .iter()
        .filter(|link| !wired.contains_key(**link))
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
        .map(|link| link.as_str())
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

    Ok(declared
        .iter()
        .map(|link| (*link, wired[link].clone()))
        .collect())
}

/// Translate a `LauncherOrigin` into a concrete on-disk path.
///
/// `Fs` is a no-op; `Repository` looks up the launcher in the cache and, for git-sourced
/// entries, materializes the checkout via `ensure_checkout`. Progress lines emitted by the
/// (blocking) checkout STREAM to the launch feedback topic as they happen, so a slow clone
/// keeps the CLI watchdog fed for the whole download. `activity_notify: None` is correct
/// here: no daemon idle watcher runs during resolve; the streaming exists for the CLI
/// watchdog, which resets on any published feedback.
async fn resolve_launcher_origin(
    ctx: &StackChangeContext,
    origin: &LauncherOrigin,
) -> std::result::Result<PathBuf, String> {
    match origin {
        LauncherOrigin::Fs(path) => Ok(path.clone()),
        LauncherOrigin::Repository { name } => {
            let peppy_dirs = ctx.peppy_dirs.clone();
            let name_for_blocking = name.clone();
            let (feedback_tx, forwarder) = spawn_feedback_forwarder(
                &ctx.feedback_publisher,
                LaunchFeedbackStep::LauncherStep,
                &ctx.log,
                None,
                None,
            );

            // The sender moves into the blocking closure, so the channel
            // closes when the checkout ends and the forwarder drains out.
            let result = tokio::task::spawn_blocking(move || {
                crate::services::repo::cache::resolve_repo_launcher_path(
                    &name_for_blocking,
                    &peppy_dirs,
                    &unlogged_step_sink(feedback_tx),
                )
            })
            .await
            .map_err(|e| format!("launcher resolver join error: {e}"))?;

            let _ = forwarder.await;
            result
        }
    }
}

/// Resolves every deployment, once, on this daemon.
///
/// The coordinator decides which bytes the whole launch runs: every
/// deployment resolves its `name:tag` to a transitive closure here, every
/// resolved item becomes a pin, and the contract and pairing documents every
/// manifest names are pinned beside them. Peers receive those pins and never
/// resolve a name, so their cache freshness, repository priorities and
/// exclusions cannot change what this launch runs.
///
/// Runs BEFORE the federated preflight: resolution touches no other machine
/// and tears nothing down, so every refusal here is free, and the pins must
/// exist before a reservation can carry them.
pub(in crate::services::stack) async fn resolve_deployments(
    ctx: &StackChangeContext,
    deployments: Vec<Deployment>,
    placements: &Placements,
) -> std::result::Result<Vec<PlannedDeployment>, String> {
    publish_stdout(
        ctx,
        format!("Resolving {} deployment(s)", deployments.len()),
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    if deployments.is_empty() {
        return Ok(Vec::new());
    }

    let node_entries = load_node_index(ctx).await?;

    let mut planned: Vec<PlannedDeployment> = Vec::new();
    let mut planning_errors: Vec<String> = Vec::new();
    let mut planned_keys: HashSet<NodeKey> = HashSet::new();

    for deployment in deployments {
        if deployment.instances.is_empty() {
            planning_errors.push(format!(
                "deployment {} must have at least one instance",
                deployment.source.label()
            ));
            continue;
        }

        let item = match resolve_planned(ctx, deployment, placements, &node_entries).await {
            Ok(item) => item,
            Err(err) => {
                planning_errors.push(err);
                continue;
            }
        };

        // The resolved identity is the source's own: the cache matches
        // `name:tag` exactly and `materialize_pinned_node` refuses bytes
        // declaring anything else, so there is one identity to name here.
        let key = item.key();
        if !planned_keys.insert(key.clone()) {
            planning_errors.push(match &item.deployment.source {
                DeploymentSource::Node { .. } => {
                    format!("duplicate deployment for node {}", key.label())
                }
                // An identical exposure set synthesizes the same identity,
                // so the second listing collides here; naming the set is
                // what points the author at the launcher line to merge.
                DeploymentSource::Exposures { .. } => format!(
                    "duplicate deployment: {} are already deployed as {}; to serve one set on \
                     several hosts, declare one deployment with one instance per host",
                    item.deployment.source.label(),
                    key.label()
                ),
            });
            continue;
        }

        publish_stdout(
            ctx,
            format!("Deployment {} resolved", key.label()),
            LaunchFeedbackStep::LauncherStep,
        )
        .await;

        planned.push(item);
    }

    if !planning_errors.is_empty() {
        let msg = daemon_config::format_bulleted(&planning_errors);
        publish_error(ctx, msg.clone(), LaunchFeedbackStep::LauncherStep).await;
        return Err(msg);
    }

    Ok(planned)
}

/// The node index, loaded ONCE for a whole resolution. Every deployment
/// resolves against the same one, and every materialization shares it
/// rather than copying it, so a resolution pays one read of a file that
/// lists every node this machine's repositories publish. A failure is
/// published as the change's error.
pub(in crate::services::stack) async fn load_node_index(
    ctx: &StackChangeContext,
) -> std::result::Result<Arc<Vec<repo_cache::NodeCacheEntry>>, String> {
    match repo_cache::load_node_cache(&ctx.peppy_dirs) {
        Ok(entries) => Ok(Arc::new(entries)),
        Err(e) => {
            let msg = format!("failed to load nodes cache: {e}");
            publish_error(ctx, msg.clone(), LaunchFeedbackStep::LauncherStep).await;
            Err(msg)
        }
    }
}

/// Resolves one deployment to the node it runs, with that node's pins and
/// the fingerprint of its manifest. The doc pins come later, from
/// [`mint_doc_pins`].
///
/// Resolution runs partly on blocking threads; progress lines stream
/// through a per-deployment forwarder (the same seam
/// `resolve_launcher_origin` uses) so a slow clone shows live progress
/// instead of a silent stretch that trips the CLI watchdog. The forwarder is
/// drained before this returns, so the deployment's progress lines come
/// before any line its caller publishes about it. Single channel, single
/// consumer: lines stay in emission order.
pub(in crate::services::stack) async fn resolve_planned(
    ctx: &StackChangeContext,
    deployment: Deployment,
    placements: &Placements,
    node_entries: &Arc<Vec<repo_cache::NodeCacheEntry>>,
) -> std::result::Result<PlannedDeployment, String> {
    let (feedback_tx, forwarder) = spawn_feedback_forwarder(
        &ctx.feedback_publisher,
        LaunchFeedbackStep::LauncherStep,
        &ctx.log,
        None,
        None,
    );
    let resolved = resolve_one(ctx, &deployment, placements, node_entries, &feedback_tx).await;
    drop(feedback_tx);
    let _ = forwarder.await;
    let ResolvedDeployment {
        config,
        root,
        closure_pins,
        pin_manifests,
        addressing,
        targets,
    } = resolved?;
    let config_sha256 = crate::services::node::manifest_fingerprint(&config)?;
    Ok(PlannedDeployment {
        deployment,
        node_name: config.manifest.name.as_str().to_owned(),
        node_tag: config.manifest.tag.clone(),
        config,
        config_sha256,
        root,
        closure_pins,
        pin_manifests,
        addressing,
        targets,
    })
}

/// What one resolved deployment contributes to the plan, before its identity
/// and doc pins are folded in. Named fields rather than a tuple:
/// `PlannedDeployment` copies it field by field, so a pin slot that swapped
/// with another would otherwise do so silently.
struct ResolvedDeployment {
    config: config::node::NodeConfig,
    /// The root pin of a node's resolved closure, or the pinned exposures
    /// of the built-in MCP server.
    root: DeploymentRoot,
    /// For a node, the dependency-node pins of its closure; for the built-in
    /// server, the contract pins its exposures reference.
    closure_pins: Vec<PinnedItem>,
    /// Every manifest in a node's closure, from which the doc pins are
    /// minted after the graph is validated. Empty for the built-in server,
    /// whose contracts are pinned with it.
    pin_manifests: Vec<config::node::Manifest>,
    /// How the deployment's node reads the sets its slots hold: the
    /// built-in server's comes from its plan, and a node reads them whole.
    addressing: daemon_config::launcher::MemberAddressing,
    /// The targets the built-in server's exposures serve, from its plan;
    /// empty for a node.
    targets: daemon_config::mcp_deployment::DeploymentTargets,
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
    ctx: &StackChangeContext,
    deployment: &Deployment,
    placements: &Placements,
    node_entries: &Arc<Vec<repo_cache::NodeCacheEntry>>,
    feedback_tx: &tokio::sync::mpsc::UnboundedSender<FeedbackLine>,
) -> std::result::Result<ResolvedDeployment, String> {
    let label = deployment.source.label();
    let remote = has_remote_instances(deployment, placements);
    let feedback: crate::services::node::cache::MaterializeFeedback =
        Arc::new(unlogged_step_sink(feedback_tx.clone()));

    let (name, tag) = match &deployment.source {
        DeploymentSource::Node { name, tag } => (name, tag),
        DeploymentSource::Exposures { exposures } => {
            publish_stdout(
                ctx,
                format!("Resolving {label}"),
                LaunchFeedbackStep::LauncherStep,
            )
            .await;
            // Cache reads and possibly a checkout: off the async runtime,
            // like every other materialization.
            let dirs = ctx.peppy_dirs.clone();
            let references = exposures.clone();
            let resolved = tokio::task::spawn_blocking(move || {
                crate::services::mcp::resolve_exposure_deployment(&dirs, &references, &|line| {
                    feedback(line)
                })
            })
            .await
            .map_err(|e| format!("deployment {label}: resolution task failed: {e}"))?
            .map_err(|e| format!("deployment {label}: {e}"))?;
            let root = DeploymentRoot::Exposures(resolved.exposure_pins());
            let closure_pins = resolved.contract_pins();
            if remote {
                refuse_unportable_pins(&label, root.items().chain(closure_pins.iter()))?;
            }
            // The contracts are pinned here, from the exposures; the
            // synthesized manifest names them again with the same pins, so
            // it is not walked by the doc-pin minting.
            return Ok(ResolvedDeployment {
                config: resolved.plan.config,
                root,
                closure_pins,
                pin_manifests: Vec::new(),
                addressing: resolved.plan.addressing,
                targets: resolved.plan.targets,
            });
        }
    };

    publish_stdout(
        ctx,
        format!("Retrieving node config for {label}"),
        LaunchFeedbackStep::LauncherStep,
    )
    .await;
    let closure = pins::resolve_pinned_closure(&ctx.peppy_dirs, node_entries, name, tag, feedback)
        .await
        .map_err(|failure| format!("deployment {label}: {}", failure.message()))?;
    if remote {
        refuse_unportable_pins(&label, closure.nodes.iter().map(|node| &node.pin))?;
    }
    let dep_pins = closure.dep_pins();
    let manifests = closure.manifests();
    // The closure is owned here, so the root comes out by move: its
    // config and pin are the two largest things in it.
    let mut nodes = closure.nodes;
    let root = nodes.remove(0);
    Ok(ResolvedDeployment {
        config: root.config,
        root: DeploymentRoot::Node(root.pin),
        closure_pins: dep_pins,
        pin_manifests: manifests,
        addressing: daemon_config::launcher::MemberAddressing::WholeSet,
        targets: daemon_config::mcp_deployment::DeploymentTargets::default(),
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
pub(in crate::services::stack) async fn mint_doc_pins(
    ctx: &StackChangeContext,
    planned: &mut [PlannedDeployment],
    placements: &Placements,
) -> std::result::Result<(), String> {
    let minted = mint_doc_pins_of(ctx, planned).await?;
    let mut problems: Vec<String> = Vec::new();
    for (item, minted) in planned.iter_mut().zip(minted) {
        let label = item.deployment.source.label();
        let doc_pins = match minted {
            Ok(doc_pins) => doc_pins,
            Err(reason) => {
                problems.push(format!("deployment {label}: {reason}"));
                continue;
            }
        };
        if has_remote_instances(&item.deployment, placements)
            && let Err(reason) = refuse_unportable_pins(&label, doc_pins.iter())
        {
            problems.push(reason);
            continue;
        }
        item.closure_pins.extend(doc_pins);
    }
    if !problems.is_empty() {
        let msg = daemon_config::format_bulleted(&problems);
        publish_error(ctx, msg.clone(), LaunchFeedbackStep::LauncherStep).await;
        return Err(msg);
    }
    Ok(())
}

/// The contract and pairing document pins of each deployment of `planned`,
/// in its order, or why one cannot be minted. The minting's progress lines
/// are published, and a failure of the caches themselves is published as the
/// change's error.
pub(in crate::services::stack) async fn mint_doc_pins_of(
    ctx: &StackChangeContext,
    planned: &[PlannedDeployment],
) -> std::result::Result<Vec<std::result::Result<Vec<PinnedItem>, String>>, String> {
    // Every deployment mints against ONE load of the contract and pairing
    // caches, on one blocking thread: both are read and parsed per load, and
    // a launch with N deployments otherwise paid N of each, sequentially, on
    // the critical path.
    let collected = Arc::new(Mutex::new(Vec::<String>::new()));
    let sets: Vec<Vec<config::node::Manifest>> = planned
        .iter()
        .map(|item| item.pin_manifests.clone())
        .collect();
    let minted = pins::doc_pins_for_manifest_sets_async(&ctx.peppy_dirs, sets, {
        let sink = Arc::clone(&collected);
        Arc::new(move |report: Report<'_>| {
            if let Report::Step(line) = report {
                sink.lock().push(line.to_owned());
            }
        })
    })
    .await;
    let captured: Vec<String> = std::mem::take(&mut *collected.lock());
    for line in captured {
        publish_stdout(ctx, line, LaunchFeedbackStep::LauncherStep).await;
    }
    if let Err(reason) = &minted {
        publish_error(ctx, reason.clone(), LaunchFeedbackStep::LauncherStep).await;
    }
    minted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::stack::fixtures::{placements_with, test_root_pin};
    use daemon_config::launcher::{DeploymentInstance, DeploymentSource};
    use daemon_config::repository::EntryOrigin;

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

    /// A deployment with every instance on the coordinator obliges its pins
    /// to nothing; one with any instance elsewhere obliges every pin it
    /// resolves to to be readable off this machine.
    #[test]
    fn only_off_coordinator_instances_oblige_portable_pins() {
        let local_only = deployment(r#"{ name: "planner", tag: "v1" }"#, &["planner_1"]);
        let placements = placements_with("cn-robot", &[("cam_cloud", "cn-atlas")]);
        assert!(!has_remote_instances(&local_only, &placements));

        let split = deployment(
            r#"{ name: "planner", tag: "v1" }"#,
            &["planner_1", "cam_cloud"],
        );
        assert!(has_remote_instances(&split, &placements));
    }

    /// The refusal the portability rule produces: a filesystem repository
    /// entry behind an off-coordinator placement names a tree only this
    /// machine can read, and the message says what still works.
    #[test]
    fn a_filesystem_backed_pin_is_refused_for_a_remote_placement() {
        let fs_pin = PinnedItem {
            origin: EntryOrigin::Fs {
                path: "/repo/planner/peppy.json5".into(),
            },
            ..test_root_pin("planner")
        };
        let error = refuse_unportable_pins("planner:v1", std::iter::once(&fs_pin))
            .expect_err("a filesystem origin cannot cross machines");
        assert!(error.contains("coordinator's filesystem"), "got: {error}");
        assert!(error.contains("git repository"), "got: {error}");

        let git_pin = test_root_pin("planner");
        assert!(refuse_unportable_pins("planner:v1", std::iter::once(&git_pin)).is_ok());
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
                .map(|(link, target)| {
                    (
                        config::runtime::Name::new(*link).unwrap(),
                        target.parse().unwrap(),
                    )
                })
                .collect(),
        )
    }

    fn no_copies() -> BTreeMap<CopyName, CoreNodeName> {
        BTreeMap::new()
    }

    /// The case the CLI cannot serve: `--local` against a launcher whose
    /// document only the coordinator has read. Expanding here is what makes
    /// `peppy stack launch --local <name>` work at all: a repository launcher
    /// is the shape every hub launcher uses, and the CLI never sees it.
    #[test]
    fn local_wires_every_declared_link_to_the_coordinator() {
        let wired = wire_core_node_links(
            &PlacementSpec::Local,
            &declared(&["robot_onboard", "cloud_inference"]),
            &no_copies(),
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
            &no_copies(),
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
            &no_copies(),
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
            &no_copies(),
            &core_node("cn-robot-7"),
        )
        .expect_err("a partial wiring must be refused");
        assert!(error.contains("cloud_inference"), "got: {error}");
        assert!(error.contains("--local"), "got: {error}");
    }

    /// Every declared link is wired here, so the only thing left to object to
    /// is the extra one, which is what isolates this rule from the
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
            &no_copies(),
            &core_node("cn-robot-7"),
        )
        .expect_err("only declared links may be wired");
        assert!(error.contains("does not declare"), "got: {error}");
        assert!(error.contains("typo_onboard"), "got: {error}");
    }

    #[test]
    fn placing_against_a_launcher_that_declares_nothing_is_refused() {
        let placement = places(&[("robot_onboard", "cn-robot-7")]);
        let error = wire_core_node_links(
            &placement,
            &declared(&[]),
            &no_copies(),
            &core_node("cn-robot-7"),
        )
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
            &no_copies(),
            &core_node("cn-robot-7"),
        )
        .expect_err("an unwired declared link must be refused");
        assert!(error.contains("declared but not wired"), "got: {error}");
    }

    // --- Placement of copies and of their parts ---

    /// A launcher whose `robot` axis runs as copies: a robot is an arm, its
    /// base, and a commander, a part its own axis brings. The file lists
    /// the copy `bravo`.
    fn robots() -> daemon_config::launcher::PreparedLauncher {
        let document = r#"{
            peppy_schema: "launcher/v1",
            components: [
                { name: "robot", cardinality: "zero_or_more", options: {
                    arm: {
                        deployments: [
                            { source: { name: "arm", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] },
                            { commander: "pad" },
                        ],
                        components: [
                            { name: "commander", options: {
                                pad: { deployments: [
                                    { source: { name: "pad", tag: "v1" }, instances: [
                                        { instance_id: "commander_inst", links: { arm: "arm_inst" } }
                                    ] }
                                ] },
                            } },
                        ],
                    },
                } },
            ],
            deployments: [{ robot: "arm", instances: [{ instance_id: "bravo" }] }],
        }"#;
        let parsed = daemon_config::launcher::PeppyLauncherParser::from_content(document)
            .expect("the launcher parses");
        daemon_config::launcher::PreparedLauncher::load(
            &parsed,
            std::path::Path::new("robots.json5"),
        )
        .expect("it loads")
    }

    /// The placements of a launch of `robots` with `words`, `joins` and
    /// `placement`, on the coordinator `cn-desk`.
    fn launch_placements(
        placement: PlacementSpec,
        words: &[&str],
        joins: &[&str],
    ) -> LaunchPlacements {
        let words: Vec<core_node_api::encoding::LaunchWord> =
            words.iter().map(|word| word.parse().unwrap()).collect();
        let joins: Vec<core_node_api::encoding::CopyReference> =
            joins.iter().map(|join| join.parse().unwrap()).collect();
        let prepared = robots();
        let composed = prepared
            .launch(&words, &joins)
            .expect("the launch composes");
        let goal = LaunchGoal::new(
            LauncherOrigin::repository("robots"),
            "launch-1",
            core_node_api::encoding::StackBudgets::new(1, 1, 1, None),
        )
        .with_layout(placement, words, joins)
        .expect("the layout holds");
        resolve_placements(&composed, &goal, "cn-desk").expect("the placements resolve")
    }

    fn machine_of(placements: &LaunchPlacements, instance: &str) -> String {
        placements.placements.of(instance).to_owned()
    }

    fn copy_machine(placements: &LaunchPlacements, copy: &str) -> String {
        placements.copy_core_nodes[&CopyName::new(copy).unwrap()].to_string()
    }

    /// A `--join` copy runs on the machine its reference names, a copy of the
    /// file where `--place` puts it, and each part of a copy where its word
    /// says; everything else runs with its copy's base.
    #[test]
    fn copies_and_their_parts_run_where_they_are_named() {
        let placements = launch_placements(
            places(&[("bravo", "cn-jetson-2")]),
            &["alpha.pad@cn-gpu-box"],
            &["arm:alpha@cn-jetson-1"],
        );
        assert_eq!(copy_machine(&placements, "alpha"), "cn-jetson-1");
        assert_eq!(machine_of(&placements, "alpha_arm_inst"), "cn-jetson-1");
        assert_eq!(
            machine_of(&placements, "alpha_commander_inst"),
            "cn-gpu-box"
        );
        assert_eq!(copy_machine(&placements, "bravo"), "cn-jetson-2");
        assert_eq!(machine_of(&placements, "bravo_arm_inst"), "cn-jetson-2");
        assert_eq!(
            machine_of(&placements, "bravo_commander_inst"),
            "cn-jetson-2"
        );
    }

    /// `self` travels as the coordinator, which a `--join`, a word and a
    /// `--place` value all resolve to.
    #[test]
    fn the_coordinator_resolves_to_this_daemons_name_everywhere() {
        let placements = launch_placements(
            places(&[("bravo", "self")]),
            &["alpha.pad@self", "bravo.pad@cn-gpu-box"],
            &["arm:alpha@self"],
        );
        assert_eq!(copy_machine(&placements, "alpha"), "cn-desk");
        assert_eq!(machine_of(&placements, "alpha_commander_inst"), "cn-desk");
        assert_eq!(copy_machine(&placements, "bravo"), "cn-desk");
        assert_eq!(machine_of(&placements, "bravo_arm_inst"), "cn-desk");
        assert_eq!(
            machine_of(&placements, "bravo_commander_inst"),
            "cn-gpu-box"
        );
        assert!(placements.placements.is_federated());

        // Nothing named: every copy, its base and its parts, on the coordinator.
        let placements = launch_placements(PlacementSpec::default(), &[], &["arm:alpha"]);
        assert_eq!(copy_machine(&placements, "alpha"), "cn-desk");
        assert_eq!(copy_machine(&placements, "bravo"), "cn-desk");
        assert!(!placements.placements.is_federated());
    }

    /// Each copy's machine is the one its name places, whatever machine runs
    /// the instance of the copy that starts first: here only the part runs
    /// away from the copy, which keeps its own machine.
    #[test]
    fn each_copy_keeps_its_own_machine() {
        let placements = launch_placements(
            PlacementSpec::default(),
            &["alpha.pad@cn-gpu-box", "bravo.pad@cn-gpu-box"],
            &["arm:alpha@cn-jetson-1"],
        );
        assert_eq!(copy_machine(&placements, "alpha"), "cn-jetson-1");
        assert_eq!(copy_machine(&placements, "bravo"), "cn-desk");
        assert_eq!(
            machine_of(&placements, "bravo_commander_inst"),
            "cn-gpu-box"
        );
    }
}
