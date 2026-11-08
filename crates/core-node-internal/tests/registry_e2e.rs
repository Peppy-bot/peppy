//! The publish of container nodes to an OCI registry and their pull, against
//! a real CNCF distribution registry (`PEPPY_TEST_REGISTRY_BIN`). The
//! publish tests build real containers, as `container_e2e` does.
//!
//! An OCI registry holds the artifacts of each peppy version under tags that
//! name it, so these tests need a peppy built with `PEPPY_GIT_TAG` set:
//! `PEPPY_GIT_TAG=v0.0.0-registry-e2e cargo test -p core-node --features
//! registry_e2e --test registry_e2e`.

#[cfg(feature = "registry_e2e")]
#[path = "."]
mod registry_e2e_tests {
    mod common;

    use common::{
        FeedbackDrainOutcome, GOAL_TIMEOUT, GoalEnd, NodeAddSource, NodeRunTestTimeouts,
        StartedCoreNode, drain_feedback, fetch_end, goal_answer, send_daemon_goal,
        send_node_add_and_wait, send_node_run_and_wait, setup_budget,
        start_core_node_publishing_to, start_core_node_with_mock_messenger,
    };
    use config::failure_kind::PeppyFailureKind;
    use config::node::Toolchain;
    use core_node_api::encoding::{
        ArtifactPlatform, ArtifactVersion, NodeBuildFeedback, NodeBuildGoal,
        NodeBuildGoalResponse, NodeBuildResult, NodeInitRequest, NodePublishRequest,
        OciRepository, PublishOutcome, PublishRefusals, RepoRefreshFeedback, RepoRefreshGoal,
        RepoRefreshGoalResponse, RepoRefreshResult,
    };
    use daemon_config::consts::{HOST_ARTIFACT_PLATFORM, artifact_version};
    use oci_registry::manifest::{
        BlobDescriptor, EMPTY_CONFIG, NodeArtifact, NodeArtifactManifest,
    };
    use oci_registry::test_support::{LocalRegistry, RegistryAuth};
    use oci_registry::{Access, CredentialStore, RegistryClient};
    use std::ops::ControlFlow;
    use std::path::Path;
    use std::time::Duration;

    const REPOSITORY_PATH: &str = "acme/peppy-nodes";

    /// How long a container build or a pull of a test may take.
    const BUILD_TIMEOUT: Duration = Duration::from_secs(900);

    /// The version of the artifacts of this peppy.
    fn version() -> ArtifactVersion {
        artifact_version().unwrap_or_else(|reason| {
            panic!(
                "the registry tests need a peppy built with PEPPY_GIT_TAG set: {reason}"
            )
        })
    }

    fn other_platform() -> ArtifactPlatform {
        ArtifactPlatform::ALL
            .into_iter()
            .find(|platform| *platform != HOST_ARTIFACT_PLATFORM)
            .expect("two platforms")
    }

    fn repository(registry: &LocalRegistry) -> OciRepository {
        OciRepository::parse(&registry.repository_url(REPOSITORY_PATH)).expect("repository")
    }

    /// Writes `repositories.json5` with the oci repository of `registry`
    /// alone, and with an fs repository at a higher id when there is one.
    fn configure_repositories(
        started: &StartedCoreNode,
        registry: &LocalRegistry,
        fs_repository: Option<&Path>,
    ) {
        let mut entries = vec![serde_json::json!({
            "id": 900, "type": "oci", "url": repository(registry).to_string()
        })];
        if let Some(path) = fs_repository {
            entries.push(serde_json::json!({
                "id": 2000, "type": "fs", "path": path.to_string_lossy()
            }));
        }
        common::write_repositories_json5(
            started,
            &serde_json::to_string(&entries).expect("serialize the repositories"),
        );
    }

    /// Runs a refresh to its end and gives its result and its feedback.
    async fn refresh(started: &StartedCoreNode) -> (RepoRefreshResult, Vec<RepoRefreshFeedback>) {
        let mut handle = send_daemon_goal(
            &started.caller_handle,
            &started.core_node_name,
            &RepoRefreshGoal,
            GOAL_TIMEOUT,
        )
        .await
        .expect("the refresh goal is sent");
        goal_answer(&handle, RepoRefreshGoalResponse::decode, "refresh goal")
            .expect("the goal answer decodes")
            .unwrap_or_else(|refusal| panic!("the refresh is refused: {refusal}"));
        let deadline = tokio::time::Instant::now() + BUILD_TIMEOUT;
        let mut feedback = Vec::new();
        let drained = drain_feedback(
            &mut handle,
            |payload| RepoRefreshFeedback::decode(payload).expect("refresh feedback decodes"),
            |line| {
                feedback.push(line);
                ControlFlow::Continue(())
            },
            deadline,
        )
        .await;
        assert!(!matches!(drained, FeedbackDrainOutcome::TimedOut));
        let end = fetch_end(
            &started.caller_handle,
            &handle,
            deadline.saturating_duration_since(tokio::time::Instant::now()),
            RepoRefreshResult::decode,
            "refresh result",
        )
        .await
        .expect("the refresh ends");
        (end.completed(), feedback)
    }

    /// Sends the build goal of `node:v1` with a publish of `extra` platforms,
    /// and follows it to its end.
    async fn publish(
        started: &StartedCoreNode,
        node: &str,
        extra: Vec<ArtifactPlatform>,
    ) -> GoalEnd<NodeBuildResult> {
        let goal = NodeBuildGoal::new(node, "v1", BUILD_TIMEOUT.as_secs()).with_publish(
            NodePublishRequest {
                repository: None,
                extra_platforms: extra,
                refusals: PublishRefusals::FailGoal,
            },
        );
        let mut handle = send_daemon_goal(
            &started.caller_handle,
            &started.core_node_name,
            &goal,
            GOAL_TIMEOUT,
        )
        .await
        .expect("the build goal is sent");
        if let Err(refusal) = goal_answer(&handle, NodeBuildGoalResponse::decode, "build goal")
            .expect("the goal answer decodes")
        {
            return GoalEnd::Refused(refusal);
        }
        let deadline = tokio::time::Instant::now() + BUILD_TIMEOUT;
        let drained = drain_feedback(
            &mut handle,
            |payload| NodeBuildFeedback::decode(payload).expect("build feedback decodes"),
            |_| ControlFlow::Continue(()),
            deadline,
        )
        .await;
        assert!(!matches!(drained, FeedbackDrainOutcome::TimedOut));
        fetch_end(
            &started.caller_handle,
            &handle,
            deadline.saturating_duration_since(tokio::time::Instant::now()),
            NodeBuildResult::decode,
            "build result",
        )
        .await
        .expect("the build ends")
    }

    /// Adds the node at `dir` from its path, which stages it again.
    async fn add_path(started: &StartedCoreNode, dir: &Path) {
        let end = send_node_add_and_wait(
            &started.caller_handle,
            &started.core_node_name,
            dir,
            GOAL_TIMEOUT,
            BUILD_TIMEOUT,
            None,
        )
        .await
        .expect("the add ends");
        assert!(end.is_completed(), "{end:?}");
    }

    /// Pushes an artifact of `node:v1` for `platform` and `artifact_version`
    /// whose `.sif` is `sif`, built from the revision `revision`, the way a
    /// publish would, without a build. Its `peppy.json5` is the one of a
    /// container node.
    async fn push_fake_artifact(
        registry: &LocalRegistry,
        node: &str,
        platform: ArtifactPlatform,
        artifact_version: &ArtifactVersion,
        revision: &str,
        sif: &[u8],
    ) {
        let client = RegistryClient::new(
            &repository(registry),
            CredentialStore::new("/nonexistent".into(), std::ffi::OsString::new()),
            Access::Push,
        )
        .expect("client");
        let dir = tempfile::tempdir().expect("tempdir");
        let sif_path = dir.path().join("node.sif");
        std::fs::write(&sif_path, sif).expect("write sif");
        let (sif_digest, sif_size) = oci_registry::digest::of_file(&sif_path).expect("hash");
        client
            .push_blob_file(&sif_path, &sif_digest, sif_size, &mut |_| {})
            .await
            .expect("push sif");
        let manifest = container_manifest(node);
        let manifest_digest = client
            .push_blob_bytes(manifest.as_bytes())
            .await
            .expect("push peppy.json5");
        client.push_blob_bytes(EMPTY_CONFIG).await.expect("push config");
        let artifact = NodeArtifactManifest {
            artifact: NodeArtifact {
                node_name: node.to_owned(),
                node_tag: "v1".to_owned(),
                version: artifact_version.clone(),
                platform,
                source_digest: oci_registry::digest::of_bytes(revision.as_bytes()),
                git_source: None,
            },
            sif: BlobDescriptor {
                digest: sif_digest,
                size: sif_size,
            },
            node_manifest: BlobDescriptor {
                digest: manifest_digest,
                size: manifest.len() as u64,
            },
        };
        client
            .put_manifest(
                &artifact.artifact.tag().expect("tag").to_string(),
                &artifact.to_bytes(),
            )
            .await
            .expect("put manifest");
    }

    fn container_manifest(node: &str) -> String {
        format!(
            r#"{{
                peppy_schema: "node/v1",
                manifest: {{ name: "{node}", tag: "v1", description: ["A test node."] }},
                interfaces: {{}},
                execution: {{ language: "python", container: {{ def_file: "apptainer.def" }} }},
            }}"#
        )
    }

    /// The node `name:v1` of the cache of `started`, as JSON.
    fn cached_node(started: &StartedCoreNode, name: &str) -> Option<serde_json::Value> {
        common::read_json5_array(&core_node::nodes_repo_cache_path(&started.peppy_dirs))
            .into_iter()
            .find(|entry| entry["node_name"] == name)
    }

    /// Publish on one daemon, refresh and pull on a second one: the same
    /// image, and the node starts. A second publish of the same revision
    /// skips, and a publish of another revision is refused, naming both
    /// revisions.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_node_published_on_one_daemon_is_pulled_and_started_on_another() {
        let version = version();
        let registry = LocalRegistry::start(RegistryAuth::Anonymous);
        let publisher = start_core_node_publishing_to(
            repository(&registry),
            Duration::from_secs(120),
            setup_budget(60),
        )
        .await;
        let nodes_root = tempfile::tempdir().expect("tempdir");
        common::poll_core_node_with_timeout(
            &publisher,
            &NodeInitRequest::new(
                nodes_root.path(),
                "hello_oci",
                config::description::Description::new("A node that greets.").unwrap(),
                common::TEST_GIT_HASH,
                true,
                Toolchain::Uv,
            ),
            Duration::from_secs(30),
        )
        .await;
        let node_dir = nodes_root.path().join("hello_oci");
        add_path(&publisher, &node_dir).await;

        let first = publish(&publisher, "hello_oci", Vec::new()).await.completed();
        let host_artifact = first.artifact_path.clone().expect("the host artifact is built");
        let [published] = first.published.as_slice() else {
            panic!("one platform: {first:?}");
        };
        assert_eq!(published.platform, HOST_ARTIFACT_PLATFORM);
        let PublishOutcome::Pushed { tag, .. } = &published.outcome else {
            panic!("pushed: {published:?}");
        };
        assert_eq!(
            tag,
            &format!("hello_oci.v1.{version}.{}", HOST_ARTIFACT_PLATFORM.tag_suffix())
        );

        add_path(&publisher, &node_dir).await;
        let again = publish(&publisher, "hello_oci", Vec::new()).await.completed();
        assert_eq!(again.artifact_path, None, "a platform published already builds nothing");
        assert!(
            matches!(again.published[0].outcome, PublishOutcome::AlreadyPublished { .. }),
            "{again:?}"
        );

        std::fs::write(node_dir.join("README.md"), "another revision").expect("change a source");
        add_path(&publisher, &node_dir).await;
        let refused = publish(&publisher, "hello_oci", Vec::new()).await;
        let failure = refused.failure();
        common::assert_failure_kind(failure, PeppyFailureKind::PreconditionFailed);
        assert!(
            failure.message().contains("holds the revision sha256:")
                && failure.message().contains("and this one is sha256:"),
            "{}",
            failure.message()
        );

        let puller = start_core_node_publishing_to(
            repository(&registry),
            Duration::from_secs(120),
            setup_budget(60),
        )
        .await;
        configure_repositories(&puller, &registry, None);
        let (refreshed, _) = refresh(&puller).await;
        assert_eq!(refreshed.total_nodes_found, 1, "{refreshed:?}");
        let entry = cached_node(&puller, "hello_oci").expect("the node is cached");
        assert_eq!(entry["origin"]["source_type"], "oci");
        let end = send_node_add_and_wait(
            &puller.caller_handle,
            &puller.core_node_name,
            NodeAddSource::ResolveRef {
                name: "hello_oci",
                tag: "v1",
            },
            GOAL_TIMEOUT,
            BUILD_TIMEOUT,
            None,
        )
        .await
        .expect("the add ends");
        assert!(end.is_completed(), "{end:?}");
        let entity = puller.node_stack.find("hello_oci", "v1").expect("added");
        let pulled = entity.read().artifact_path().expect("an image").to_path_buf();
        assert_eq!(
            oci_registry::digest::of_file(&pulled).expect("hash").0,
            oci_registry::digest::of_file(&host_artifact).expect("hash").0,
            "the pulled image is the published one"
        );

        let started = send_node_run_and_wait(
            &puller.caller_handle,
            &puller.core_node_name,
            common::default_instance_plan("hello_oci_inst"),
            "hello_oci",
            "v1",
            &NodeRunTestTimeouts {
                goal: Duration::from_secs(30),
                result: Duration::from_secs(120),
            },
            None,
        )
        .await
        .expect("the run ends");
        let instance = match started.end {
            GoalEnd::Completed(result) => result,
            other => panic!("the pulled node starts: {other:?}"),
        };
        assert!(instance.pid.is_some_and(|pid| pid > 0));
    }

    /// A refresh keeps the artifacts of its version, records a node for the
    /// platform of this machine with every platform it has, leaves a node it
    /// has no artifact of for this platform to the repository with a higher
    /// id, and refuses a node whose platforms were built from different
    /// revisions.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refresh_reads_the_artifacts_of_its_version_and_platform() {
        let version = version();
        let other_version = ArtifactVersion::parse("v0.0.0-another").expect("version");
        let registry = LocalRegistry::start(RegistryAuth::Anonymous);
        let host = HOST_ARTIFACT_PLATFORM;
        let other = other_platform();
        push_fake_artifact(&registry, "both", host, &version, "r1", b"both-host").await;
        push_fake_artifact(&registry, "both", other, &version, "r1", b"both-other").await;
        push_fake_artifact(&registry, "older", host, &other_version, "r1", b"older").await;
        push_fake_artifact(&registry, "elsewhere", other, &version, "r1", b"elsewhere").await;
        push_fake_artifact(&registry, "split", host, &version, "r1", b"split-host").await;
        push_fake_artifact(&registry, "split", other, &version, "r2", b"split-other").await;

        let fs_repository = tempfile::tempdir().expect("tempdir");
        let elsewhere_dir = fs_repository.path().join("nodes/elsewhere");
        common::write_plain_peppy_json5(
            &elsewhere_dir,
            &common::minimal_peppy_json5("elsewhere", "v1"),
        );
        common::publish_repo_index(fs_repository.path());

        let started = start_core_node_with_mock_messenger().await;
        configure_repositories(&started, &registry, Some(fs_repository.path()));
        let (_, feedback) = refresh(&started).await;

        let both = cached_node(&started, "both").expect("recorded");
        assert_eq!(both["origin"]["source_type"], "oci");
        let platforms = both["origin"]["platforms"].as_object().expect("platforms");
        assert!(platforms.contains_key(host.as_str()) && platforms.contains_key(other.as_str()));
        assert!(cached_node(&started, "older").is_none(), "another version");
        assert_eq!(
            cached_node(&started, "elsewhere").expect("from the fs repository")["origin"]
                ["source_type"],
            "fs"
        );
        assert!(cached_node(&started, "split").is_none(), "two revisions");
        let lines: Vec<String> = feedback
            .iter()
            .filter_map(|line| match line {
                RepoRefreshFeedback::Progress { message } => Some(message.clone()),
                _ => None,
            })
            .collect();
        assert!(
            lines.iter().any(|line| line.contains("refused `split:v1`")
                && line.contains(oci_registry::digest::of_bytes(b"r1").as_str())
                && line.contains(oci_registry::digest::of_bytes(b"r2").as_str())),
            "{lines:#?}"
        );

        // The node the oci repository lacks for this platform resolves from
        // the fs repository, and builds.
        let end = send_node_add_and_wait(
            &started.caller_handle,
            &started.core_node_name,
            NodeAddSource::ResolveRef {
                name: "elsewhere",
                tag: "v1",
            },
            GOAL_TIMEOUT,
            BUILD_TIMEOUT,
            None,
        )
        .await
        .expect("the add ends");
        assert!(end.is_completed(), "{end:?}");
        common::build_staged_node(&started, "elsewhere", "v1").await;
    }

    /// An image whose bytes the registry serves do not match their digest is
    /// not stored: the pull fails, names the repository, the tag and the
    /// digest, and leaves nothing in storage.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pull_of_a_tampered_image_fails_and_stores_nothing() {
        let version = version();
        let registry = LocalRegistry::start(RegistryAuth::Anonymous);
        let sif = vec![7u8; 4096];
        push_fake_artifact(&registry, "tampered", HOST_ARTIFACT_PLATFORM, &version, "r1", &sif)
            .await;
        let digest = oci_registry::digest::of_bytes(&sif);
        let blob = registry.storage_dir().join(format!(
            "docker/registry/v2/blobs/sha256/{}/{}/data",
            &digest.hex()[..2],
            digest.hex()
        ));
        std::fs::write(&blob, vec![8u8; 4096]).expect("tamper the stored blob");

        let started = start_core_node_with_mock_messenger().await;
        configure_repositories(&started, &registry, None);
        refresh(&started).await;
        let end = send_node_add_and_wait(
            &started.caller_handle,
            &started.core_node_name,
            NodeAddSource::ResolveRef {
                name: "tampered",
                tag: "v1",
            },
            GOAL_TIMEOUT,
            BUILD_TIMEOUT,
            None,
        )
        .await
        .expect("the add ends");
        let message = end.failure_message().to_owned();
        assert!(message.contains(&repository(&registry).to_string()), "{message}");
        assert!(message.contains(digest.as_str()), "{message}");
        assert!(message.contains("tampered.v1."), "{message}");
        let stored = started.peppy_dirs.built_node_dir("tampered", "v1");
        let left: Vec<_> = std::fs::read_dir(&stored)
            .map(|entries| entries.flatten().map(|entry| entry.file_name()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "nothing is stored: {left:?}");
    }
}
