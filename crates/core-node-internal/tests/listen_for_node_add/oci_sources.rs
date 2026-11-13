//! The add of a node of an oci origin: no staging, no generation, the image
//! of the platform of this machine. These tests reach no registry: an image
//! already in storage is not pulled again, and a missing platform fails
//! before any pull.

use super::*;
use common::{TestPackagesCache, test_sif_digest};
use core_node_api::encoding::{
    ArtifactPlatform, NodeBuildGoal, NodeBuildGoalResponse, OciRepository,
};
use daemon_config::consts::HOST_ARTIFACT_PLATFORM;

const REPOSITORY: &str = "oci://registry.example/acme/peppy-nodes";

/// The `peppy.json5` of a container node, as its artifact holds it.
fn container_manifest(name: &str, tag: &str) -> String {
    format!(
        r#"{{
            peppy_schema: "node/v1",
            manifest: {{
                name: "{name}",
                tag: "{tag}",
                description: ["A test node."],
            }},
            interfaces: {{}},
            execution: {{
                language: "rust",
                container: {{
                    def_file: "apptainer.def",
                }}
            }}
        }}"#
    )
}

/// The platform that is not the one of this machine.
fn other_platform() -> ArtifactPlatform {
    ArtifactPlatform::ALL
        .into_iter()
        .find(|platform| *platform != HOST_ARTIFACT_PLATFORM)
        .expect("two platforms")
}

async fn add(started: &StartedCoreNode, name: &str, tag: &str) -> GoalEnd<NodeAddResult> {
    send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::ResolveRef { name, tag },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("the add should end")
}

/// A machine whose platform the oci origin holds no artifact for cannot run
/// the node: the add fails, names the node, the platform and the command
/// that publishes one, and pulls nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oci_node_without_an_artifact_for_this_platform_is_refused() {
    let started = start_core_node_with_mock_messenger().await;
    TestPackagesCache::new()
        .oci_entry(
            "waldo",
            "v1",
            REPOSITORY,
            &container_manifest("waldo", "v1"),
            &[other_platform()],
        )
        .write(&started.peppy_dirs);

    let end = add(&started, "waldo", "v1").await;

    let failure = end.failure();
    common::assert_failure_kind(failure, PeppyFailureKind::PreconditionFailed);
    let message = failure.message();
    assert!(
        message.contains("`waldo:v1` comes from the oci repository"),
        "{message}"
    );
    assert!(message.contains(REPOSITORY), "{message}");
    assert!(
        message.contains(HOST_ARTIFACT_PLATFORM.as_str()),
        "{message}"
    );
    assert!(
        message.contains(&format!(
            "peppy node publish waldo:v1 --platforms {HOST_ARTIFACT_PLATFORM}"
        )),
        "{message}"
    );
    assert!(!started.node_stack.contains("waldo", "v1"));
    assert!(
        !started.peppy_dirs.built_node_dir("waldo", "v1").exists(),
        "nothing is pulled"
    );
}

/// An image already in storage is not pulled again: the add registers the
/// node ready to start, from the image of its own platform, and a later
/// `node build` and `node build --rebuild` refuse the node, which has no
/// sources here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oci_node_is_added_ready_from_its_image_and_refuses_a_build() {
    let started = start_core_node_with_mock_messenger().await;
    TestPackagesCache::new()
        .oci_entry(
            "waldo",
            "v1",
            REPOSITORY,
            &container_manifest("waldo", "v1"),
            &ArtifactPlatform::ALL,
        )
        .write(&started.peppy_dirs);
    let image = node_stack::pulled_artifact_path(
        &started.peppy_dirs,
        "waldo",
        "v1",
        &test_sif_digest("waldo", "v1", HOST_ARTIFACT_PLATFORM),
        HOST_ARTIFACT_PLATFORM,
    )
    .expect("image path");
    std::fs::create_dir_all(image.parent().expect("dir")).expect("mkdir");
    std::fs::write(&image, b"SIF").expect("seed the pulled image");

    let end = add(&started, "waldo", "v1").await;

    end.completed();
    let entity = started.node_stack.find("waldo", "v1").expect("added");
    assert_eq!(entity.read().stage().name(), "Ready");
    assert_eq!(entity.read().artifact_path(), Some(image.as_path()));
    assert_eq!(
        entity.read().pulled_from(),
        Some(&OciRepository::parse(REPOSITORY).expect("url"))
    );
    assert!(
        !started.peppy_dirs.tmp_dir().join("waldo").exists(),
        "nothing is staged"
    );

    for rebuild in [false, true] {
        let goal =
            NodeBuildGoal::new("waldo", "v1", RESULT_TIMEOUT.as_secs()).with_rebuild(rebuild);
        let handle = common::send_daemon_goal(
            &started.caller_handle,
            &started.core_node_name,
            &goal,
            GOAL_TIMEOUT,
        )
        .await
        .expect("the build goal should be sent");
        let refusal = common::goal_answer(&handle, NodeBuildGoalResponse::decode, "build goal")
            .expect("the goal answer should decode")
            .expect_err("a build of a pulled node is refused");
        common::assert_failure_kind(&refusal, PeppyFailureKind::PreconditionFailed);
        assert_eq!(
            refusal.message(),
            format!(
                "`waldo:v1` comes from the oci repository `{REPOSITORY}` and has no sources. \
                 To build it, add a git or fs repository that holds it, with a lower id."
            ),
            "rebuild: {rebuild}"
        );
    }
}
