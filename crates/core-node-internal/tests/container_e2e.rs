#[cfg(feature = "container_e2e")]
#[path = "."]
mod container_e2e_tests {
    mod common;

    use common::{
        CALLER_INSTANCE_ID, NodeRunTestTimeouts, send_node_add_and_wait, send_node_build_and_wait,
        send_node_build_and_wait_forced, send_node_run_and_wait,
        start_core_node_with_real_messenger_and_timeouts, write_peppy_json5,
    };
    use config::consts::DEFAULT_ALPINE_BASE_IMAGE;
    use config::node::Name as NodeName;
    use config::node::Toolchain;
    use core_node_api::encoding::NodeInitRequest;
    use node_stack::{NodeStack, NodeStage};
    use peppylib::core_node::transport::poll_node_init;
    use std::time::Duration;
    use tempfile::tempdir;

    /// End-to-end test: init a Rust container node, build the container image,
    /// and start it using the real Apptainer runtime.
    ///
    /// Exercises the full chain: NodeInitRequest (with_container=true) ->
    /// node_add (apptainer build) -> node_run (apptainer run).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn container_e2e_rust_init_add_start() {
        const NODE_NAME: &str = "rust_e2e_node";
        const NODE_TAG: &str = "v1";
        const INSTANCE_ID: &str = "rust_e2e_instance";

        let started = start_core_node_with_real_messenger_and_timeouts(
            Duration::from_secs(120),
            Duration::from_secs(60),
        )
        .await;

        // Step 1: Init the node with container support
        let nodes_root = tempdir().expect("failed to create temp nodes root directory");

        let init_response = poll_node_init(
            &NodeInitRequest::new(
                nodes_root.path(),
                NODE_NAME,
                "test-hash",
                true,
                Toolchain::Cargo,
            ),
            &started.caller_handle,
            &started.core_node_name,
            CALLER_INSTANCE_ID,
            &started.core_node_name,
            Duration::from_secs(30),
        )
        .await
        .expect("node_init request should complete");

        assert!(
            init_response.success,
            "node_init should succeed, got error: {}",
            init_response.error_message
        );

        let node_dir = nodes_root.path().join(NODE_NAME);
        assert!(
            node_dir.join("apptainer.def").exists(),
            "init should generate apptainer.def"
        );

        // Step 2: Add the node (builds the container image).
        let add_response = send_node_add_and_wait(
            &started.caller_handle,
            &started.core_node_name,
            &node_dir,
            Duration::from_secs(30),
            Duration::from_secs(600),
            None,
        )
        .await
        .expect("node_add request should complete");

        assert!(
            add_response.success,
            "node_add should succeed, got error: {:?}",
            add_response.error_message
        );

        // Step 3: Build the node (builds the container image).
        let build_response = send_node_build_and_wait(
            &started.caller_handle,
            &started.core_node_name,
            NODE_NAME,
            NODE_TAG,
            Duration::from_secs(30),
            Duration::from_secs(600),
            Vec::new(),
            None,
        )
        .await
        .expect("node_build request should complete");

        assert!(
            build_response.success,
            "node_build (container build) should succeed, got error: {:?}",
            build_response.error_message
        );

        // Step 4: Start the container against a real messaging endpoint.
        // This mirrors real world usage and avoids mocked ready/health responders.
        let (messaging_host, messaging_port) = started
            .caller_handle
            .messaging_endpoint()
            .await
            .expect("zenoh endpoint should be available");

        let runtime_config_json5 = common::build_runtime_config_json5(
            messaging_host.as_str(),
            messaging_port,
            &started.core_node_name,
            NODE_NAME,
            NODE_TAG,
            INSTANCE_ID,
            Default::default(),
        );

        let start_response = send_node_run_and_wait(
            &started.caller_handle,
            &started.core_node_name,
            &runtime_config_json5,
            NODE_NAME,
            NODE_TAG,
            &NodeRunTestTimeouts {
                goal: Duration::from_secs(30),
                result: Duration::from_secs(60),
            },
            None,
        )
        .await
        .expect("node_run action should complete");

        assert!(
            start_response.result.success,
            "node_run should succeed, got error: {:?}",
            start_response.result.error_message
        );

        assert!(
            start_response.result.pid.is_some(),
            "node_run should return a PID on success"
        );
        assert!(
            start_response.result.pid.unwrap() > 0,
            "node_run PID should be a positive number"
        );

        let instance_id = NodeName::new(INSTANCE_ID).expect("valid instance id");
        assert!(
            started
                .node_stack
                .find_by_instance_id(&instance_id)
                .is_some(),
            "instance should be registered in the node stack"
        );

        assert!(
            start_response.goal_response.accepted,
            "goal should be accepted"
        );

        let log_path = &start_response.goal_response.log_path;
        assert!(log_path.exists(), "log file should exist at {:?}", log_path);

        let log_content =
            std::fs::read_to_string(log_path).expect("should be able to read log file");
        assert!(
            log_content.contains("Executing apptainer run"),
            "log file should contain apptainer run command, got:\n{}",
            log_content
        );
        assert!(
            !log_content.contains("CodegenFingerprintRead"),
            "log file should not contain config fingerprint startup errors, got:\n{}",
            log_content
        );
    }

    /// End-to-end test: init a Python container node, build the container image,
    /// and start it using the real Apptainer runtime.
    ///
    /// Exercises the full chain: NodeInitRequest (with_container=true) ->
    /// node_add (apptainer build) -> node_run (apptainer run).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn container_e2e_python_init_add_start() {
        const NODE_NAME: &str = "python_e2e_node";
        const NODE_TAG: &str = "v1";
        const INSTANCE_ID: &str = "python_e2e_instance";

        let started = start_core_node_with_real_messenger_and_timeouts(
            Duration::from_secs(120),
            Duration::from_secs(60),
        )
        .await;

        // Step 1: Init the node with container support
        let nodes_root = tempdir().expect("failed to create temp nodes root directory");

        let init_response = poll_node_init(
            &NodeInitRequest::new(
                nodes_root.path(),
                NODE_NAME,
                "test-hash",
                true,
                Toolchain::Uv,
            ),
            &started.caller_handle,
            &started.core_node_name,
            CALLER_INSTANCE_ID,
            &started.core_node_name,
            Duration::from_secs(30),
        )
        .await
        .expect("node_init request should complete");

        assert!(
            init_response.success,
            "node_init should succeed, got error: {}",
            init_response.error_message
        );

        let node_dir = nodes_root.path().join(NODE_NAME);
        assert!(
            node_dir.join("apptainer.def").exists(),
            "init should generate apptainer.def"
        );

        // Step 2: Add the node (builds the container image).
        let add_response = send_node_add_and_wait(
            &started.caller_handle,
            &started.core_node_name,
            &node_dir,
            Duration::from_secs(30),
            Duration::from_secs(300),
            None,
        )
        .await
        .expect("node_add request should complete");

        assert!(
            add_response.success,
            "node_add should succeed, got error: {:?}",
            add_response.error_message
        );

        // Step 3: Build the node (builds the container image).
        let build_response = send_node_build_and_wait(
            &started.caller_handle,
            &started.core_node_name,
            NODE_NAME,
            NODE_TAG,
            Duration::from_secs(30),
            Duration::from_secs(300),
            Vec::new(),
            None,
        )
        .await
        .expect("node_build request should complete");

        assert!(
            build_response.success,
            "node_build (container build) should succeed, got error: {:?}",
            build_response.error_message
        );

        // Step 4: Start the container against a real messaging endpoint.
        // This mirrors real world usage and avoids mocked ready/health responders.
        let (messaging_host, messaging_port) = started
            .caller_handle
            .messaging_endpoint()
            .await
            .expect("zenoh endpoint should be available");

        let runtime_config_json5 = common::build_runtime_config_json5(
            messaging_host.as_str(),
            messaging_port,
            &started.core_node_name,
            NODE_NAME,
            NODE_TAG,
            INSTANCE_ID,
            Default::default(),
        );

        let start_response = send_node_run_and_wait(
            &started.caller_handle,
            &started.core_node_name,
            &runtime_config_json5,
            NODE_NAME,
            NODE_TAG,
            &NodeRunTestTimeouts {
                goal: Duration::from_secs(30),
                result: Duration::from_secs(60),
            },
            None,
        )
        .await
        .expect("node_run action should complete");

        assert!(
            start_response.result.success,
            "node_run should succeed, got error: {:?}",
            start_response.result.error_message
        );

        assert!(
            start_response.result.pid.is_some(),
            "node_run should return a PID on success"
        );
        assert!(
            start_response.result.pid.unwrap() > 0,
            "node_run PID should be a positive number"
        );

        let instance_id = NodeName::new(INSTANCE_ID).expect("valid instance id");
        assert!(
            started
                .node_stack
                .find_by_instance_id(&instance_id)
                .is_some(),
            "instance should be registered in the node stack"
        );

        assert!(
            start_response.goal_response.accepted,
            "goal should be accepted"
        );

        let log_path = &start_response.goal_response.log_path;
        assert!(log_path.exists(), "log file should exist at {:?}", log_path);

        let log_content =
            std::fs::read_to_string(log_path).expect("should be able to read log file");
        assert!(
            log_content.contains("Executing apptainer run"),
            "log file should contain apptainer run command, got:\n{}",
            log_content
        );
        assert!(
            !log_content.contains("CodegenFingerprintRead"),
            "log file should not contain config fingerprint startup errors, got:\n{}",
            log_content
        );
    }

    /// Polls the node stack until `(name, tag)` is `Building`, so the next
    /// `--force` actually supersedes a running build rather than racing its
    /// admission.
    async fn wait_until_building(stack: &NodeStack, name: &str, tag: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(handle) = stack.find(name, tag)
                && matches!(handle.read().stage(), NodeStage::Building { .. })
            {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "node {name}:{tag} did not enter Building within 60s"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// `node build --force` superseding an in-flight container build must SIGKILL
    /// the displaced build's process group (apptainer + its `%post` children),
    /// not just the host `limactl`. Regression test for the guest zombie left
    /// behind when the PGID file write lost a guest-filesystem race and the kill
    /// became a no-op.
    ///
    /// Runs on both backends: `guest_command` queries the kernel the build runs
    /// in (the Lima guest on macOS, the host on Linux), and the assertion is the
    /// same. The bug is macOS-only, so this is red on the pre-fix Lima path and a
    /// green regression guard on Linux (where cancellation already uses the host
    /// process group).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn container_force_build_kills_displaced_guest_build() {
        const NODE_NAME: &str = "zombie_e2e_node";
        const NODE_TAG: &str = "v1";
        const MARKER: &str = "PEPPY_ZOMBIE_E2E_MARKER";
        // Each `--force` supersedes the prior in-flight build. The first build is
        // cold (its PGID write always succeeds), so we need several reuse builds
        // for the guest-FS race to bite at least once.
        const BUILDS: usize = 6;

        let started = start_core_node_with_real_messenger_and_timeouts(
            Duration::from_secs(120),
            Duration::from_secs(60),
        )
        .await;

        // Same runtime the build uses, for inspecting/cleaning its processes.
        let facade = containers::Apptainer::new().expect("apptainer facade should init");

        // Count live `%post` marker processes in the build kernel. The `[P]...`
        // class keeps the grep from matching its own argv.
        let count_markers = || -> usize {
            let out = facade
                .guest_command(&[
                    "sh",
                    "-c",
                    "ps -eo args | grep -c '[P]EPPY_ZOMBIE_E2E_MARKER'",
                ])
                .expect("guest_command should run");
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse()
                .unwrap_or(0)
        };

        // A container node whose apptainer build blocks forever in `%post`,
        // leaving a uniquely-named process (`sh /PEPPY_ZOMBIE_E2E_MARKER`, so the
        // marker shows up in `ps args`) for each in-flight build.
        let source_dir = tempfile::tempdir().expect("source dir");
        write_peppy_json5(
            source_dir.path(),
            r#"{
                peppy_schema: "node_v1",
                manifest: { name: "zombie_e2e_node", tag: "v1" },
                execution: { language: "rust", container: { def_file: "apptainer.def" } }
            }"#,
        );
        std::fs::write(
            source_dir.path().join("apptainer.def"),
            format!(
                "Bootstrap: docker\nFrom: {DEFAULT_ALPINE_BASE_IMAGE}\n\n\
                 %post\n    echo 'while true; do sleep 100000; done' > /{MARKER}\n    sh /{MARKER}\n"
            ),
        )
        .expect("write apptainer.def");

        // Stage the node (node_add does not build the image).
        let add = send_node_add_and_wait(
            &started.caller_handle,
            &started.core_node_name,
            source_dir.path(),
            Duration::from_secs(30),
            Duration::from_secs(300),
            None,
        )
        .await
        .expect("node_add request should complete");
        assert!(
            add.success,
            "node_add should succeed, got error: {:?}",
            add.error_message
        );

        // Fire the builds. Each blocks in `%post` (so it never returns until
        // superseded) and each `--force` supersedes the prior one, which must
        // kill the displaced build's whole process group.
        let mut tasks = Vec::new();
        for i in 0..BUILDS {
            let messenger = started.caller_handle.clone();
            let core_node = started.core_node_name.clone();
            tasks.push(tokio::spawn(async move {
                let _ = send_node_build_and_wait_forced(
                    &messenger,
                    &core_node,
                    NODE_NAME,
                    NODE_TAG,
                    Duration::from_secs(30),
                    Duration::from_secs(600),
                    Vec::new(),
                    None,
                )
                .await;
            }));

            // Wait until the build is registered Building, then let apptainer
            // fetch/extract and enter `%post` (its marker process appears).
            wait_until_building(&started.node_stack, NODE_NAME, NODE_TAG).await;
            tokio::time::sleep(Duration::from_secs(12)).await;
            eprintln!(
                "[zombie-e2e] after build {i}: live markers = {}",
                count_markers()
            );
        }

        // After the supersede chain, at most the final still-active build may have
        // a live marker. On the pre-fix code, displaced reuse-builds whose PGID
        // write lost the race were never killed and leak orphaned `%post`
        // processes.
        let live = count_markers();

        // Clean up before asserting so a failure never leaks build processes.
        // Kill each marker's whole (setsid-detached) process group so the
        // `%post` loop and its `sleep` child go together.
        let kill_groups = format!(
            "for p in $(pgrep -f '[P]{m}'); do \
                 g=$(ps -o pgid= -p \"$p\" 2>/dev/null | tr -d ' '); \
                 [ -n \"$g\" ] && kill -KILL -\"$g\" 2>/dev/null; \
             done; true",
            m = &MARKER[1..]
        );
        let _ = facade.guest_command(&["sh", "-c", &kill_groups]);
        for t in tasks {
            t.abort();
        }

        assert!(
            live <= 1,
            "after {BUILDS} `--force` rebuilds, {live} `%post` marker processes survived in \
             the build kernel; every displaced build must be SIGKILLed (expected <= 1, only \
             the active build)"
        );
    }
}
