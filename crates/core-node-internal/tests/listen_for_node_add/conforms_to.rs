//! Regression tests for `node_add` with `interfaces.conforms_to`.
//!
//! `node_add` copies the source to a temp working dir and *regenerates*
//! peppygen there before handing off to `node_build`. An earlier version
//! of the add path called only `collect_consumed_interfaces` (which
//! handles `depends_on`) and skipped `resolve_conforms_to`, so the
//! generator never saw the conformed interface's topics/services. The
//! resulting peppygen had a flat layout (`emitted_topics.rs` was empty)
//! and any node code importing nested paths like
//! `peppygen::emitted_topics::<iface>::<tag>::<topic>` failed to compile
//! inside the container. `sync` did this correctly, which is why a sync
//! followed by build was enough to mask the bug on a developer's
//! workstation but the daemon-driven path produced broken artifacts.
//!
//! These tests assert against the daemon-staged working dir directly
//! (via `pending_working_dir`) so they catch a regression in the add
//! path even when `sync` would still pass.
use super::*;
use common::TestPackagesCache;

const INTERFACE_BODY: &str = r#"{
    peppy_schema: "interface/v1",
    manifest: { name: "uvc_camera", tag: "v1" },
    interfaces: {
        topics: [
            { name: "video_stream", qos_profile: "sensor_data" }
        ],
        services: [
            { name: "video_stream_info" }
        ]
    }
}"#;

const NODE_BODY: &str = r#"{
    peppy_schema: "node/v1",
    manifest: { name: "fake_uvc_camera", tag: "v1" },
    interfaces: {
        conforms_to: [
            { name: "uvc_camera", tag: "v1" }
        ]
    },
    execution: {
        language: "rust",
        run_cmd: ["sleep", "10"]
    }
}"#;

/// `node_add` must resolve `conforms_to` and pass the conformed
/// topics/services to the generator so the staged peppygen nests
/// artifacts under `{category}/{iface_name}/{iface_tag}/`. Before the
/// fix, the working dir's `emitted_topics.rs` and
/// `exposed_services.rs` were empty for a node whose only contributions
/// came through `conforms_to`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_add_generates_conformed_interface_modules_in_working_dir() {
    let started = start_core_node_with_mock_messenger().await;
    let node_stack = started.node_stack.clone();
    let peppy_dirs = started.peppy_dirs.clone();

    // Stage the interface on disk plus an fs-backed cache entry so
    // `resolve_conforms_to` can find it.
    let iface_dir = TempDir::new().expect("iface tempdir");
    let iface_path = iface_dir.path().join("uvc_camera.json5");
    std::fs::write(&iface_path, INTERFACE_BODY).expect("write interface");

    TestPackagesCache::new()
        .interface_fs_entry("uvc_camera", "v1", &iface_path, INTERFACE_BODY)
        .write(&peppy_dirs);

    let node_dir = TempDir::new().expect("node tempdir");
    write_peppy_json5(node_dir.path(), NODE_BODY);

    let add_result = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        node_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(
        add_result.success,
        "node_add should resolve conforms_to and succeed, got error: {:?}",
        add_result.error_message
    );

    // The add path stashes its temp working dir on the entity so a
    // later `node_build` can reuse the prepared sources. Inspect that
    // staged copy; that's the directory apptainer will copy into the
    // container, so it's the one that must contain the nested
    // peppygen modules.
    let entity = node_stack
        .find("fake_uvc_camera", "v1")
        .expect("entity should be in the stack after add");
    let working_dir = entity
        .read()
        .pending_working_dir()
        .expect("add should have staged a working dir for the build phase")
        .path()
        .to_path_buf();

    let peppygen_src = working_dir.join(PEPPYGEN_OUTPUT_PATH).join("src");

    let emitted_topics = std::fs::read_to_string(peppygen_src.join("emitted_topics.rs"))
        .expect("emitted_topics.rs should exist in staged peppygen");
    assert!(
        emitted_topics.contains("pub mod uvc_camera;"),
        "emitted_topics.rs should declare the conformed interface module \
         `uvc_camera`; got:\n{emitted_topics}"
    );
    assert!(
        peppygen_src
            .join("emitted_topics/uvc_camera/v1/video_stream.rs")
            .is_file(),
        "expected nested `emitted_topics/uvc_camera/v1/video_stream.rs` \
         in staged peppygen at {}",
        peppygen_src.display(),
    );

    let exposed_services = std::fs::read_to_string(peppygen_src.join("exposed_services.rs"))
        .expect("exposed_services.rs should exist in staged peppygen");
    assert!(
        exposed_services.contains("pub mod uvc_camera;"),
        "exposed_services.rs should declare the conformed interface module \
         `uvc_camera`; got:\n{exposed_services}"
    );
    assert!(
        peppygen_src
            .join("exposed_services/uvc_camera/v1/video_stream_info.rs")
            .is_file(),
        "expected nested `exposed_services/uvc_camera/v1/video_stream_info.rs` \
         in staged peppygen at {}",
        peppygen_src.display(),
    );
}
