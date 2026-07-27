use super::feedback::{publish_stderr, publish_stdout};
use super::{NodeKey, PlannedDeployment, ProcessLaunchContext};
use crate::services::node::resolve_node_config;
use core_node_api::encoding::{
    LaunchFeedbackStep, LaunchGoal, LaunchResult, LauncherOrigin, NodeSource,
};
use daemon_config::consts::PeppyDirs;
use daemon_config::launcher::{Deployment, DeploymentSource, PeppyLauncherParser, Placements};
use parking_lot::Mutex as StdMutex;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Translates a deployment source into one that any daemon in the federation
/// can fetch for itself.
///
/// Two things make this different from
/// [`node_source_from_deployment_source`], and both are about not letting one
/// machine's filesystem leak into another's instructions:
///
/// - `local:` is REFUSED. Its path names a tree on the coordinator, so a peer
///   resolving it would read a different tree or nothing at all.
/// - `repo:` stays a `(name, tag)` reference instead of being resolved through
///   the caller's own package cache. Resolving it here would pin whatever git
///   URL or archive THIS machine happens to have cached and hand it to another
///   machine as fact. Left as a reference, each daemon resolves it against its
///   own cache, which is the same cache it will spawn from.
///
/// Called on both sides for that reason: by the coordinator when it builds a
/// peer's `node_add`, and by the peer when it resolves its own manifests. Same
/// function, so the two cannot drift into disagreeing about what a source means.
pub(crate) fn portable_node_source(
    source: &DeploymentSource,
) -> std::result::Result<NodeSource, String> {
    match source {
        DeploymentSource::Local(spec) => Err(format!(
            "`local:{}` cannot be placed on another core node: the path names a tree on the \
             coordinator's filesystem. A `local:` deployment must keep all of its instances on \
             one core node; publish the node to a repo or url source to split it across machines.",
            spec.local.display()
        )),
        DeploymentSource::Repo(spec) => NodeSource::repo_node(&spec.name, &spec.tag)
            .map_err(|e| format!("invalid repo node `{}:{}`: {e}", spec.name, spec.tag)),
        DeploymentSource::Git(spec) => Ok(NodeSource::Git {
            repo_url: git_url_from_repo(&spec.repo)?,
            repo_path: spec.path.clone(),
            repo_ref: Some(spec.ref_.clone()),
        }),
        DeploymentSource::Url(spec) => Ok(NodeSource::Http {
            url: url::Url::parse(&spec.url)
                .map_err(|e| format!("invalid HTTP URL `{}`: {e}", spec.url))?,
            sha256: Some(spec.sha256.clone()),
        }),
    }
}

fn deployment_label(deployment: &Deployment) -> String {
    match &deployment.source {
        DeploymentSource::Local(spec) => format!("local:{}", spec.local.display()),
        DeploymentSource::Git(spec) => format!("git:{}@{}:{}", spec.repo, spec.ref_, spec.path),
        DeploymentSource::Url(spec) => format!("url:{}", spec.url),
        DeploymentSource::Repo(spec) => format!("repo:{}:{}", spec.name, spec.tag),
    }
}

fn git_url_from_repo(repo: &str) -> std::result::Result<gix_url::Url, String> {
    gix_url::Url::try_from(repo)
        .or_else(|_| gix_url::Url::try_from(std::path::Path::new(repo)))
        .map_err(|e| format!("invalid git repo URL `{repo}`: {e}"))
}

pub(crate) fn node_source_from_deployment_source(
    deployment: &Deployment,
    nodes_directory: &std::path::Path,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<NodeSource, String> {
    // Only the two host-dependent shapes are handled here; the rest are
    // identical from any machine and come from `portable_node_source`, so the
    // two functions cannot drift apart on what a `git:` or `url:` source means.
    match &deployment.source {
        DeploymentSource::Local(spec) => Ok(NodeSource::Fs(if spec.local.is_absolute() {
            spec.local.clone()
        } else {
            nodes_directory.join(&spec.local)
        })),
        DeploymentSource::Repo(spec) => {
            crate::services::repo::cache::resolve_repo_node_source(&spec.name, &spec.tag, peppy_dirs)
        }
        portable => portable_node_source(portable),
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

/// Binds the launcher's declared core node links to the machines named by
/// `--place`, then resolves every instance to the core node it runs on.
///
/// The coordinator owns this, not the CLI. The CLI passes the raw pairs
/// through and renders feedback; only the daemon has the resolved document,
/// which matters because a `Repository` launcher (the shape every hub launcher
/// uses) is resolved from the daemon's own repo cache and the CLI may never
/// have seen it.
pub(super) fn resolve_placements(
    launcher: &daemon_config::launcher::PeppyLauncher,
    goal: &LaunchGoal,
    coordinator: &str,
) -> std::result::Result<Placements, String> {
    let declared: BTreeSet<&str> = launcher.core_nodes.iter().map(String::as_str).collect();

    if declared.is_empty() && !goal.core_node_links.is_empty() {
        let wired = goal
            .core_node_links
            .keys()
            .map(|link| format!("`{link}`"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "--place was given ({wired}) but this launcher declares no `core_nodes`. Either \
             remove --place, or add a `core_nodes` list naming the machines the launcher spans."
        ));
    }

    // Every declared link must be wired exactly once, and only declared links
    // may be wired. A launcher describes a topology; refusing a partial wiring
    // is what stops half of it from silently collapsing onto the coordinator.
    let mut missing: Vec<&str> = Vec::new();
    for link in &declared {
        if !goal.core_node_links.contains_key(*link) {
            missing.push(link);
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "these core node links are declared but not wired: {}. Wire each one with \
             `--place <core-node-link>@<core-node>`, or run the whole launcher on this machine \
             with `--local`.",
            missing
                .iter()
                .map(|link| format!("`{link}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let undeclared: Vec<&str> = goal
        .core_node_links
        .keys()
        .map(String::as_str)
        .filter(|link| !declared.contains(link))
        .collect();
    if !undeclared.is_empty() {
        return Err(format!(
            "--place names core node links this launcher does not declare: {}. It declares: {}",
            undeclared
                .iter()
                .map(|link| format!("`{link}`"))
                .collect::<Vec<_>>()
                .join(", "),
            if declared.is_empty() {
                "nothing".to_owned()
            } else {
                declared
                    .iter()
                    .map(|link| format!("`{link}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ));
    }

    let mut by_instance = BTreeMap::new();
    for deployment in &launcher.deployments {
        for instance in &deployment.instances {
            let Some(link) = &instance.core_node else {
                continue;
            };
            // The parser already refused a `core_node` naming an undeclared
            // link, and every declared link is wired by the checks above.
            let core_node = goal
                .core_node_links
                .get(link)
                .expect("every declared link is wired and every core_node names a declared link");
            by_instance.insert(instance.instance_id.to_string(), core_node.clone());
        }
    }

    Ok(Placements::new(coordinator, by_instance))
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

/// Step 2: Resolve deployments - retrieve node configs for each deployment.
///
/// A deployment placed wholly on a peer is NOT resolved here. The peer already
/// resolved it during preflight and `delegated` carries what it read, so this
/// daemon needs no reachability to a source it will never fetch, and the
/// manifest it validates is provably the one that peer will spawn from.
pub(super) async fn resolve_deployments(
    ctx: &ProcessLaunchContext,
    deployments: Vec<Deployment>,
    nodes_directory: &Path,
    delegated: &BTreeMap<usize, super::federated::DelegatedManifest>,
) -> std::result::Result<Vec<PlannedDeployment>, LaunchResult> {
    publish_stdout(
        ctx,
        format!("Resolving {} deployment(s)", deployments.len()),
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    let mut planned: Vec<PlannedDeployment> = Vec::new();
    let mut planning_errors: Vec<String> = Vec::new();
    let mut planned_keys: HashSet<NodeKey> = HashSet::new();

    for (index, deployment) in deployments.into_iter().enumerate() {
        if deployment.instances.is_empty() {
            planning_errors.push(format!(
                "deployment {} must have at least one instance",
                deployment_label(&deployment)
            ));
            continue;
        }

        let resolved = match delegated.get(&index) {
            Some(manifest) => adopt_delegated_manifest(ctx, &deployment, manifest).await,
            None => resolve_here(ctx, &deployment, nodes_directory).await,
        };
        let (source, config, manifest_sha256) = match resolved {
            Ok(resolved) => resolved,
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
            deployment_index: index,
            manifest_sha256,
        });
    }

    if !planning_errors.is_empty() {
        let msg = daemon_config::format_bulleted(&planning_errors);
        publish_stderr(ctx, msg.clone(), LaunchFeedbackStep::LauncherStep).await;
        return Err(LaunchResult::failure(&ctx.log_path, msg));
    }

    Ok(planned)
}

/// What one resolved deployment contributes to the plan: how to fetch it, what
/// its manifest says, and this daemon's own fingerprint of that manifest (only
/// when this daemon read it, so a straddle can be cross-checked).
type ResolvedDeployment = (NodeSource, config::node::NodeConfig, Option<String>);

/// Takes a peer's resolved manifest at face value, but still derives the
/// portable source: what the coordinator needs from it is the instruction to
/// send back, not a path on the peer's disk.
async fn adopt_delegated_manifest(
    ctx: &ProcessLaunchContext,
    deployment: &Deployment,
    manifest: &super::federated::DelegatedManifest,
) -> std::result::Result<ResolvedDeployment, String> {
    publish_stdout(
        ctx,
        format!(
            "Using the manifest `{}` resolved for {}",
            manifest.core_node,
            deployment_label(deployment)
        ),
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    let config: config::node::NodeConfig = serde_json5::from_str(&manifest.config_json5)
        .map_err(|e| {
            format!(
                "`{}` sent an undecodable manifest for deployment {}: {e}",
                manifest.core_node,
                deployment_label(deployment)
            )
        })?;
    let source = portable_node_source(&deployment.source)?;
    // No local fingerprint: this daemon did not read the manifest, so it has
    // nothing of its own to compare, and the straddle check must not compare a
    // peer's answer against itself.
    Ok((source, config, None))
}

async fn resolve_here(
    ctx: &ProcessLaunchContext,
    deployment: &Deployment,
    nodes_directory: &Path,
) -> std::result::Result<ResolvedDeployment, String> {
    let source = node_source_from_deployment_source(deployment, nodes_directory, &ctx.peppy_dirs)
        .map_err(|err| {
            format!(
                "failed to resolve source for deployment {}: {err}",
                deployment_label(deployment)
            )
        })?;

    publish_stdout(
        ctx,
        format!(
            "Retrieving node config for {}",
            deployment_label(deployment)
        ),
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    let config = resolve_node_config(source.clone(), &ctx.peppy_dirs)
        .await
        .map_err(|err| {
            format!(
                "failed to retrieve node config for deployment {}: {err}",
                deployment_label(deployment)
            )
        })?;
    let fingerprint = crate::services::node::manifest_fingerprint(&config)?;
    Ok((source, config, Some(fingerprint)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(json5: &str) -> DeploymentSource {
        serde_json5::from_str(json5).expect("valid deployment source")
    }

    /// The hazard this exists to avoid: resolving `repo:` through the caller's
    /// own package cache would pin whatever git URL THIS machine happens to
    /// have cached and hand it to another machine as fact. Left as a reference,
    /// each daemon resolves it against the cache it will actually spawn from.
    #[test]
    fn a_repo_source_crosses_machines_as_a_reference_not_a_resolved_url() {
        let resolved = portable_node_source(&source(r#"{ name: "planner", tag: "v1" }"#))
            .expect("a repo source is portable");
        assert_eq!(
            resolved,
            NodeSource::RepoNode {
                name: "planner".to_owned(),
                tag: "v1".to_owned(),
            }
        );
    }

    #[test]
    fn git_and_url_sources_name_the_same_bytes_from_any_machine() {
        assert!(matches!(
            portable_node_source(&source(
                r#"{ repo: "https://example.com/r.git", ref: "main", path: "nodes/a" }"#
            )),
            Ok(NodeSource::Git { .. })
        ));
        assert!(matches!(
            portable_node_source(&source(
                r#"{ url: "https://example.com/a.tar.zst", sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" }"#
            )),
            Ok(NodeSource::Http { .. })
        ));
    }

    /// A path names a tree on the coordinator's disk, so a peer resolving it
    /// would read a different tree or nothing at all.
    #[test]
    fn a_local_source_is_refused_rather_than_translated() {
        let error = portable_node_source(&source(r#"{ local: "./nodes/planner" }"#))
            .expect_err("a local path cannot cross machines");
        assert!(error.contains("names a tree on"), "got: {error}");
        assert!(error.contains("repo or url source"), "got: {error}");
    }

    /// The local resolver and the portable one must not drift apart on what a
    /// host-independent source means, which is why the former delegates to the
    /// latter for exactly those shapes.
    #[test]
    fn the_local_resolver_agrees_with_the_portable_one_on_host_independent_sources() {
        for json5 in [
            r#"{ repo: "https://example.com/r.git", ref: "main", path: "nodes/a" }"#,
            r#"{ url: "https://example.com/a.tar.zst", sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" }"#,
        ] {
            let deployment = Deployment {
                source: source(json5),
                instances: Vec::new(),
            };
            let local = node_source_from_deployment_source(
                &deployment,
                std::path::Path::new("/nodes"),
                &PeppyDirs::new("/tmp/peppy"),
            )
            .expect("resolvable locally");
            let portable = portable_node_source(&deployment.source).expect("portable");
            assert_eq!(local, portable, "for {json5}");
        }
    }
}
