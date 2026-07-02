use super::feedback::{publish_stderr, publish_stdout};
use super::{NodeKey, PlannedDeployment, ProcessLaunchContext};
use crate::services::node::resolve_node_config;
use core_node_api::encoding::{
    LaunchFeedbackStep, LaunchGoal, LaunchResult, LauncherOrigin, NodeSource,
};
use daemon_config::consts::PeppyDirs;
use daemon_config::launcher::{Deployment, DeploymentSource, PeppyLauncherParser};
use parking_lot::Mutex as StdMutex;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

fn node_source_from_deployment_source(
    deployment: &Deployment,
    nodes_directory: &std::path::Path,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<NodeSource, String> {
    let source = match &deployment.source {
        DeploymentSource::Local(spec) => {
            let resolved = if spec.local.is_absolute() {
                spec.local.clone()
            } else {
                nodes_directory.join(&spec.local)
            };
            NodeSource::Fs(resolved)
        }
        DeploymentSource::Git(spec) => {
            let repo_url = git_url_from_repo(&spec.repo)?;
            NodeSource::Git {
                repo_url,
                repo_path: spec.path.clone(),
                repo_ref: Some(spec.ref_.clone()),
            }
        }
        DeploymentSource::Url(spec) => {
            let url = url::Url::parse(&spec.url)
                .map_err(|e| format!("invalid HTTP URL `{}`: {e}", spec.url))?;
            NodeSource::Http {
                url,
                sha256: Some(spec.sha256.clone()),
            }
        }
        DeploymentSource::Repo(spec) => crate::services::repo::cache::resolve_repo_node_source(
            &spec.name, &spec.tag, peppy_dirs,
        )?,
    };

    Ok(source)
}

/// Collapse a per-instance launcher override and the daemon-wide default
/// into a single resolved framework block. Centralizes the "per-instance
/// value > daemon default > wall" precedence so the spawned node receives
/// one concrete value and never has to re-implement the fallback.
pub(super) fn resolve_framework(
    overrides: &daemon_config::launcher::FrameworkOverrides,
    daemon_default_use_sim_time: bool,
) -> config::runtime::ResolvedFramework {
    config::runtime::ResolvedFramework {
        use_sim_time: overrides
            .use_sim_time
            .unwrap_or(daemon_default_use_sim_time),
    }
}

/// Step 1: Parse launcher configuration from file path.
pub(super) async fn parse_launcher_config(
    ctx: &ProcessLaunchContext,
    goal: &LaunchGoal,
) -> std::result::Result<(Vec<Deployment>, PathBuf), LaunchResult> {
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

    let deployments = peppy_launcher.deployments.clone();
    Ok((deployments, nodes_directory))
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
pub(super) async fn resolve_deployments(
    ctx: &ProcessLaunchContext,
    deployments: Vec<Deployment>,
    nodes_directory: &Path,
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

    for deployment in deployments.into_iter() {
        if deployment.instances.is_empty() {
            planning_errors.push(format!(
                "deployment {} must have at least one instance",
                deployment_label(&deployment)
            ));
            continue;
        }

        let source =
            match node_source_from_deployment_source(&deployment, nodes_directory, &ctx.peppy_dirs)
            {
                Ok(result) => result,
                Err(err) => {
                    planning_errors.push(format!(
                        "failed to resolve source for deployment {}: {err}",
                        deployment_label(&deployment)
                    ));
                    continue;
                }
            };

        publish_stdout(
            ctx,
            format!(
                "Retrieving node config for {}",
                deployment_label(&deployment)
            ),
            LaunchFeedbackStep::LauncherStep,
        )
        .await;

        let config = match resolve_node_config(source.clone(), &ctx.peppy_dirs).await {
            Ok(config) => config,
            Err(err) => {
                planning_errors.push(format!(
                    "failed to retrieve node config for deployment {}: {err}",
                    deployment_label(&deployment)
                ));
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
        });
    }

    if !planning_errors.is_empty() {
        let msg = daemon_config::format_bulleted(&planning_errors);
        publish_stderr(ctx, msg.clone(), LaunchFeedbackStep::LauncherStep).await;
        return Err(LaunchResult::failure(&ctx.log_path, msg));
    }

    Ok(planned)
}
