//! `peppy node publish`: the build of a container node of the node stack
//! and the push of its `.sif` to the publish repository of the daemon.

use core_node_api::encoding::{
    ArtifactPlatform, NodeBuildFeedback, NodeBuildGoal, NodeBuildGoalResponse, NodeBuildResult,
    NodePublishRequest, PlatformPublish, PublishRefusals,
};
use std::sync::Arc;
use tracing::{info, warn};

use super::TimeoutConfig;
use super::env::caller_env_overrides;
use crate::commands::action_poll::AfterGoal;
use crate::commands::{CALLER_INSTANCE_ID, GOAL_TIMEOUT};
use crate::context::AppContext;
use crate::error::{Error, Result};
use peppylib::core_node::transport::send_goal;

pub(super) struct PublishNodeParams {
    pub node_name: String,
    pub node_tag: String,
    /// The platforms to build and push beside the one of the daemon.
    pub platforms: Vec<ArtifactPlatform>,
    /// The timeouts of each platform build.
    pub timeouts: TimeoutConfig,
    pub force: bool,
}

pub(super) fn publish_node(ctx: &Arc<AppContext>, params: PublishNodeParams) -> Result<()> {
    crate::commands::block_on(publish_node_async(ctx, params))
}

async fn publish_node_async(ctx: &Arc<AppContext>, params: PublishNodeParams) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;
    let PublishNodeParams {
        node_name,
        node_tag,
        platforms,
        timeouts,
        force,
    } = params;
    info!("Publishing node {node_name}:{node_tag}...");
    // The daemon builds the platforms one after another, each within the
    // maximum: the whole publish is bounded by the sum. The idle timeout
    // applies as it is, since every build reports its progress.
    let builds = 1 + platforms.len() as u64;
    let timeouts = TimeoutConfig {
        max_secs: timeouts.max_secs.saturating_mul(builds),
        ..timeouts
    };
    let goal = NodeBuildGoal::new(&node_name, &node_tag, timeouts.max_secs)
        .with_env_vars(caller_env_overrides())
        .with_force(force)
        .with_publish(NodePublishRequest {
            repository: None,
            extra_platforms: platforms,
            refusals: PublishRefusals::FailGoal,
        });
    let mut action_handle = send_goal(
        &goal,
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        Some(&conn.target_core_node),
        GOAL_TIMEOUT,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("Failed to send the node_build goal: {e}")))?;

    let result = crate::commands::action_poll::run_action_with_feedback::<
        NodeBuildGoalResponse,
        NodeBuildFeedback,
        NodeBuildResult,
    >(
        conn.messenger,
        &mut action_handle,
        &timeouts,
        "Node publish",
        AfterGoal::CommandEnds,
    )
    .await?;
    report_publish(&node_name, &node_tag, &result.published)
}

/// Prints what the publish did for each platform, and fails when a platform
/// of the request was not published.
fn report_publish(node_name: &str, node_tag: &str, published: &[PlatformPublish]) -> Result<()> {
    for platform in published {
        if platform.is_published() {
            info!("{platform}");
        } else {
            warn!("{platform}");
        }
    }
    let missing = published
        .iter()
        .filter(|platform| !platform.is_published())
        .count();
    if missing == 0 {
        info!("Published node {node_name}:{node_tag}");
        return Ok(());
    }
    Err(Error::ExecutionFailed(format!(
        "{missing} platform(s) of {node_name}:{node_tag} were not published"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_node_api::encoding::{NotPublishedReason, OciDigest, PublishOutcome};

    fn pushed(platform: ArtifactPlatform) -> PlatformPublish {
        PlatformPublish {
            platform,
            outcome: PublishOutcome::Pushed {
                tag: "waldo.v1.v0.36.0.linux-amd64".to_owned(),
                manifest_digest: OciDigest::from_sha256_hex(&"a".repeat(64)).unwrap(),
            },
        }
    }

    #[test]
    fn a_publish_whose_every_platform_is_in_the_repository_succeeds() {
        let already = PlatformPublish {
            platform: ArtifactPlatform::LinuxArm64,
            outcome: PublishOutcome::AlreadyPublished {
                tag: "waldo.v1.v0.36.0.linux-arm64".to_owned(),
            },
        };
        assert!(
            report_publish(
                "waldo",
                "v1",
                &[pushed(ArtifactPlatform::LinuxAmd64), already]
            )
            .is_ok()
        );
    }

    #[test]
    fn a_publish_with_a_platform_not_published_fails() {
        let refused = PlatformPublish {
            platform: ArtifactPlatform::LinuxArm64,
            outcome: PublishOutcome::NotPublished {
                reason: NotPublishedReason::BuildFailed,
                detail: "the build failed".to_owned(),
            },
        };
        let error = report_publish(
            "waldo",
            "v1",
            &[pushed(ArtifactPlatform::LinuxAmd64), refused],
        )
        .expect_err("a platform was not published");
        assert!(
            error.to_string().contains("1 platform(s) of waldo:v1"),
            "{error}"
        );
    }
}
