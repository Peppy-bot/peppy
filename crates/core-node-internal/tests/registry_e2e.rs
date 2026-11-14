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
        ArtifactPlatform, ArtifactVersion, LaunchFeedback, LaunchGoal, LaunchGoalResponse,
        LaunchResult, LauncherOrigin, NodeBuildFeedback, NodeBuildGoal, NodeBuildGoalResponse,
        NodeBuildResult, NodeInitRequest, NodePublishLine, NodePublishLineOutcome,
        NodePublishRequest, OciDigest, OciRepository, PlatformPublish, PublishOutcome,
        PublishRefusals, RepoRefreshFeedback, RepoRefreshGoal, RepoRefreshGoalResponse,
        RepoRefreshResult, StackBudgets, StackBuildGoal, StackPublish,
    };
    use daemon_config::consts::{HOST_ARTIFACT_PLATFORM, artifact_version};
    use oci_registry::manifest::{
        BlobDescriptor, EMPTY_CONFIG, NodeArtifact, NodeArtifactManifest,
    };
    use oci_registry::test_support::{LocalRegistry, RegistryAuth};
    use oci_registry::{Access, CredentialStore, RegistryClient};
    use std::assert_matches;
    use std::fmt::Display;
    use std::ops::ControlFlow;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    const REPOSITORY_PATH: &str = "acme/peppy-nodes";

    /// How long a container build or a pull of a test may take.
    const BUILD_TIMEOUT: Duration = Duration::from_mins(15);

    /// The version of the artifacts of this peppy.
    fn version() -> ArtifactVersion {
        artifact_version().unwrap_or_else(|reason| {
            panic!("the registry tests need a peppy built with PEPPY_GIT_TAG set: {reason}")
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

    /// Sends `goal`, follows its feedback to its end, and gives how it ended
    /// with every feedback line, in order. The decoders read the answer, the
    /// feedback and the result of the kind of `goal`.
    async fn run_goal<Response, ResponseError, Feedback, Outcome, OutcomeError>(
        started: &StartedCoreNode,
        goal: &impl core_node_api::ActionGoal,
        decode_response: impl Fn(&[u8]) -> Result<Response, ResponseError>,
        decode_feedback: impl Fn(&[u8]) -> Feedback,
        decode_outcome: impl Fn(&[u8]) -> Result<Outcome, OutcomeError>,
    ) -> (GoalEnd<Outcome>, Vec<Feedback>)
    where
        ResponseError: Display,
        OutcomeError: Display,
    {
        let mut handle = send_daemon_goal(
            &started.caller_handle,
            &started.core_node_name,
            goal,
            GOAL_TIMEOUT,
        )
        .await
        .expect("the goal is sent");
        if let Err(refusal) =
            goal_answer(&handle, decode_response, "goal answer").expect("the goal answer decodes")
        {
            return (GoalEnd::Refused(refusal), Vec::new());
        }
        let deadline = tokio::time::Instant::now() + BUILD_TIMEOUT;
        let mut feedback = Vec::new();
        let drained = drain_feedback(
            &mut handle,
            decode_feedback,
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
            decode_outcome,
            "goal result",
        )
        .await
        .expect("the goal ends");
        (end, feedback)
    }

    /// Runs a refresh to its end and gives its result and its feedback.
    async fn refresh(started: &StartedCoreNode) -> (RepoRefreshResult, Vec<RepoRefreshFeedback>) {
        let (end, feedback) = run_goal(
            started,
            &RepoRefreshGoal,
            RepoRefreshGoalResponse::decode,
            |payload| RepoRefreshFeedback::decode(payload).expect("refresh feedback decodes"),
            RepoRefreshResult::decode,
        )
        .await;
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
        run_goal(
            started,
            &goal,
            NodeBuildGoalResponse::decode,
            |payload| NodeBuildFeedback::decode(payload).expect("build feedback decodes"),
            NodeBuildResult::decode,
        )
        .await
        .0
    }

    /// Runs `peppy stack build --publish` of `launcher` to its end.
    async fn stack_build_publishing(
        started: &StartedCoreNode,
        launcher: &Path,
        launch_id: &str,
    ) -> GoalEnd<LaunchResult> {
        let budgets = StackBudgets::new(
            BUILD_TIMEOUT.as_secs(),
            BUILD_TIMEOUT.as_secs(),
            BUILD_TIMEOUT.as_secs(),
            None,
        );
        let goal = StackBuildGoal::from(
            LaunchGoal::new(
                LauncherOrigin::Fs(launcher.to_path_buf()),
                launch_id,
                budgets,
            )
            .with_publish(Some(StackPublish {
                extra_platforms: Vec::new(),
            })),
        );
        run_goal(
            started,
            &goal,
            LaunchGoalResponse::decode,
            |payload| LaunchFeedback::decode(payload).expect("launch feedback decodes"),
            LaunchResult::decode,
        )
        .await
        .0
    }

    /// The source digest of a revision named `seed`.
    fn revision(seed: &str) -> OciDigest {
        oci_registry::digest::of_bytes(seed.as_bytes())
    }

    /// The staged tree of the node `name:v1` of the stack of `started`.
    fn staged_tree(started: &StartedCoreNode, name: &str) -> PathBuf {
        started
            .node_stack
            .find(name, "v1")
            .expect("the node is in the stack")
            .read()
            .pending_working_dir()
            .map(|guard| guard.path().to_path_buf())
            .expect("the node has a staged tree")
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
    /// whose `.sif` is `sif`, built from the revision `source_digest`, the
    /// way a publish would, without a build. Its `peppy.json5` is the one of
    /// a container node.
    async fn push_fake_artifact(
        registry: &LocalRegistry,
        node: &str,
        platform: ArtifactPlatform,
        artifact_version: &ArtifactVersion,
        source_digest: &OciDigest,
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
        client
            .push_blob_bytes(EMPTY_CONFIG)
            .await
            .expect("push config");
        let artifact = NodeArtifactManifest {
            artifact: NodeArtifact {
                node_name: node.to_owned(),
                node_tag: "v1".to_owned(),
                version: artifact_version.clone(),
                platform,
                source_digest: source_digest.clone(),
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

    /// Adds `name:v1` from the repositories of `started`, and gives the
    /// image the node runs from.
    async fn add_pulled(started: &StartedCoreNode, name: &str) -> PathBuf {
        let end = send_node_add_and_wait(
            &started.caller_handle,
            &started.core_node_name,
            NodeAddSource::ResolveRef { name, tag: "v1" },
            GOAL_TIMEOUT,
            BUILD_TIMEOUT,
            None,
        )
        .await
        .expect("the add ends");
        assert!(end.is_completed(), "{end:?}");
        started
            .node_stack
            .find(name, "v1")
            .expect("added")
            .read()
            .artifact_path()
            .expect("an image")
            .to_path_buf()
    }

    /// The node `name:v1` of the cache of `started`, as JSON.
    fn cached_node(started: &StartedCoreNode, name: &str) -> Option<serde_json::Value> {
        common::read_json5_array(&core_node::nodes_repo_cache_path(&started.peppy_dirs))
            .into_iter()
            .find(|entry| entry["node_name"] == name)
    }

    /// Publish on one daemon, refresh and pull on a second one: the same
    /// image, and the node starts. The publish adds the platform of this
    /// machine beside the one another machine published of the same
    /// revision. A second publish of the same revision skips, and a publish
    /// of another revision is refused, naming both revisions.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_node_published_on_one_daemon_is_pulled_and_started_on_another() {
        let version = version();
        let registry = LocalRegistry::start(RegistryAuth::Anonymous);
        let publisher = start_core_node_publishing_to(
            repository(&registry),
            Duration::from_mins(2),
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
        // A machine of the other platform published this revision first.
        let source_digest =
            node_stack::source_digest(&staged_tree(&publisher, "hello_oci")).expect("digest");
        push_fake_artifact(
            &registry,
            "hello_oci",
            other_platform(),
            &version,
            &source_digest,
            b"the image of the other platform",
        )
        .await;

        let first = publish(&publisher, "hello_oci", Vec::new())
            .await
            .completed();
        let host_artifact = first
            .artifact_path
            .clone()
            .expect("the host artifact is built");
        let [published] = first.published.as_slice() else {
            panic!("one platform: {first:?}");
        };
        assert_eq!(published.platform, HOST_ARTIFACT_PLATFORM);
        let PublishOutcome::Pushed { tag, .. } = &published.outcome else {
            panic!("pushed: {published:?}");
        };
        assert_eq!(
            tag,
            &format!(
                "hello_oci.v1.{version}.{}",
                HOST_ARTIFACT_PLATFORM.tag_suffix()
            )
        );

        add_path(&publisher, &node_dir).await;
        let again = publish(&publisher, "hello_oci", Vec::new())
            .await
            .completed();
        assert_eq!(
            again.artifact_path, None,
            "a platform published already builds nothing"
        );
        assert_matches!(
            again.published[0].outcome,
            PublishOutcome::AlreadyPublished { .. },
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
            Duration::from_mins(2),
            setup_budget(60),
        )
        .await;
        configure_repositories(&puller, &registry, None);
        let (refreshed, _) = refresh(&puller).await;
        assert_eq!(refreshed.total_nodes_found, 1, "{refreshed:?}");
        let entry = cached_node(&puller, "hello_oci").expect("the node is cached");
        assert_eq!(entry["origin"]["source_type"], "oci");
        assert_eq!(entry["origin"]["source_digest"], source_digest.as_str());
        let platforms = entry["origin"]["platforms"].as_object().expect("platforms");
        assert!(
            platforms.contains_key(HOST_ARTIFACT_PLATFORM.as_str())
                && platforms.contains_key(other_platform().as_str()),
            "both platforms of the revision: {platforms:?}"
        );
        let pulled = add_pulled(&puller, "hello_oci").await;
        assert_eq!(
            oci_registry::digest::of_file(&pulled).expect("hash").0,
            oci_registry::digest::of_file(&host_artifact)
                .expect("hash")
                .0,
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
        push_fake_artifact(
            &registry,
            "both",
            host,
            &version,
            &revision("r1"),
            b"both-host",
        )
        .await;
        push_fake_artifact(
            &registry,
            "both",
            other,
            &version,
            &revision("r1"),
            b"both-other",
        )
        .await;
        push_fake_artifact(
            &registry,
            "older",
            host,
            &other_version,
            &revision("r1"),
            b"older",
        )
        .await;
        push_fake_artifact(
            &registry,
            "elsewhere",
            other,
            &version,
            &revision("r1"),
            b"elsewhere",
        )
        .await;
        push_fake_artifact(
            &registry,
            "split",
            host,
            &version,
            &revision("r1"),
            b"split-host",
        )
        .await;
        push_fake_artifact(
            &registry,
            "split",
            other,
            &version,
            &revision("r2"),
            b"split-other",
        )
        .await;

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
            cached_node(&started, "elsewhere").expect("from the fs repository")["origin"]["source_type"],
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
                && line.contains(revision("r1").as_str())
                && line.contains(revision("r2").as_str())),
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
        push_fake_artifact(
            &registry,
            "tampered",
            HOST_ARTIFACT_PLATFORM,
            &version,
            &revision("r1"),
            &sif,
        )
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
        assert!(
            message.contains(&repository(&registry).to_string()),
            "{message}"
        );
        assert!(message.contains(digest.as_str()), "{message}");
        assert!(message.contains("tampered.v1."), "{message}");
        let stored = started.peppy_dirs.built_node_dir("tampered", "v1");
        let left: Vec<_> = std::fs::read_dir(&stored)
            .map_or_default(|entries| entries.flatten().map(|entry| entry.file_name()).collect());
        assert!(left.is_empty(), "nothing is stored: {left:?}");
    }

    /// A pull stores a new image of a node beside the earlier one, and the
    /// add removes the earlier one once the node runs from the new one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_new_image_replaces_the_earlier_one_once_the_node_is_replaced() {
        let version = version();
        let registry = LocalRegistry::start(RegistryAuth::Anonymous);
        let started = start_core_node_with_mock_messenger().await;
        configure_repositories(&started, &registry, None);
        let mut images = Vec::new();
        for (seed, sif) in [("r1", "the first image"), ("r2", "the next image")] {
            push_fake_artifact(
                &registry,
                "renewed",
                HOST_ARTIFACT_PLATFORM,
                &version,
                &revision(seed),
                sif.as_bytes(),
            )
            .await;
            refresh(&started).await;
            images.push(add_pulled(&started, "renewed").await);
        }
        let [first, next] = images.as_slice() else {
            panic!("two adds: {images:?}");
        };
        assert_ne!(first, next, "each image has the path of its digest");
        assert_eq!(
            std::fs::read(next).expect("the next image"),
            b"the next image"
        );
        assert!(!first.exists(), "the earlier image is removed: {first:?}");
    }

    /// The report line of `node` among `lines`.
    fn line_of<'a>(lines: &'a [NodePublishLine], node: &str) -> &'a NodePublishLine {
        lines
            .iter()
            .find(|line| line.node_label == node)
            .unwrap_or_else(|| panic!("a report line of {node}: {lines:#?}"))
    }

    /// `peppy stack build --publish` of a launcher with a container node, a
    /// process node and a node of an oci origin: the build pushes the
    /// container node, and reports the two others, which fail nothing. A
    /// second build reports the container node as published already.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_stack_build_publishes_its_container_nodes_and_reports_the_others() {
        let version = version();
        let host = HOST_ARTIFACT_PLATFORM;
        let registry = LocalRegistry::start(RegistryAuth::Anonymous);
        push_fake_artifact(
            &registry,
            "prebuilt",
            host,
            &version,
            &revision("r1"),
            b"prebuilt",
        )
        .await;
        let started = start_core_node_publishing_to(
            repository(&registry),
            Duration::from_mins(2),
            setup_budget(60),
        )
        .await;

        let sources = tempfile::tempdir().expect("tempdir");
        common::poll_core_node_with_timeout(
            &started,
            &NodeInitRequest::new(
                sources.path(),
                "stack_hello",
                config::description::Description::new("A node that greets.").unwrap(),
                common::TEST_GIT_HASH,
                true,
                Toolchain::Uv,
            ),
            Duration::from_secs(30),
        )
        .await;
        common::write_plain_peppy_json5(
            &sources.path().join("plain"),
            &common::minimal_peppy_json5("plain", "v1"),
        );
        common::publish_repo_index(sources.path());
        configure_repositories(&started, &registry, Some(sources.path()));
        refresh(&started).await;

        let launchers = tempfile::tempdir().expect("tempdir");
        let launcher = launchers.path().join("publish.json5");
        std::fs::write(
            &launcher,
            r#"{
                peppy_schema: "launcher/v1",
                description: ["A test launcher."],
                deployments: [
                    { source: { name: "stack_hello", tag: "v1" }, instances: [{ instance_id: "hello_inst" }] },
                    { source: { name: "plain", tag: "v1" }, instances: [{ instance_id: "plain_inst" }] },
                    { source: { name: "prebuilt", tag: "v1" }, instances: [{ instance_id: "prebuilt_inst" }] },
                ],
            }"#,
        )
        .expect("write the launcher");

        let first = stack_build_publishing(&started, &launcher, "publish-first")
            .await
            .completed();
        let hello_tag = format!("stack_hello.v1.{version}.{}", host.tag_suffix());
        assert_eq!(first.published.len(), 3, "{:#?}", first.published);
        match &line_of(&first.published, "stack_hello:v1").outcome {
            NodePublishLineOutcome::Platform(PlatformPublish {
                platform,
                outcome: PublishOutcome::Pushed { tag, .. },
            }) => {
                assert_eq!(*platform, host);
                assert_eq!(tag, &hello_tag);
            }
            other => panic!("the container node is pushed: {other:?}"),
        }
        assert_eq!(
            line_of(&first.published, "plain:v1").outcome,
            NodePublishLineOutcome::ProcessNode
        );
        assert_eq!(
            line_of(&first.published, "prebuilt:v1").outcome,
            NodePublishLineOutcome::OciOrigin(repository(&registry))
        );
        assert!(
            first
                .published
                .iter()
                .all(|line| line.core_node == started.core_node_name && !line.fails_the_build()),
            "{:#?}",
            first.published
        );

        let again = stack_build_publishing(&started, &launcher, "publish-again")
            .await
            .completed();
        assert_eq!(
            line_of(&again.published, "stack_hello:v1").outcome,
            NodePublishLineOutcome::Platform(PlatformPublish {
                platform: host,
                outcome: PublishOutcome::AlreadyPublished { tag: hello_tag },
            })
        );
    }
}
