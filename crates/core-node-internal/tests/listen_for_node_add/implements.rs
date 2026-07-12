//! Regression tests for `node_add` with `manifest.implements`.
//!
//! `node_add` copies the source to a temp working dir and *regenerates*
//! peppygen there before handing off to `node_build`. An earlier version
//! of the add path called only `collect_consumed_interfaces` (which
//! handles `depends_on`) and skipped the produced-side contract resolution, so the
//! generator never saw the contract-backed's topics/services. The
//! resulting peppygen had a flat layout (`emitted_topics.rs` was empty)
//! and any node code importing nested paths like
//! `peppygen::emitted_topics::<contract>::<tag>::<topic>` failed to compile
//! inside the container. `sync` did this correctly, which is why a sync
//! followed by build was enough to mask the bug on a developer's
//! workstation but the daemon-driven path produced broken artifacts.
//!
//! These tests assert against the daemon-staged working dir directly
//! (via `pending_working_dir`) so they catch a regression in the add
//! path even when `sync` would still pass.
use super::*;
use common::TestPackagesCache;

const CONTRACT_BODY: &str = r#"{
    peppy_schema: "contract/v1",
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
    manifest: {
        name: "fake_uvc_camera", tag: "v1",
        implements: [
            { name: "uvc_camera", tag: "v1", link_id: "cam" }
        ]
    },
    interfaces: {
        topics: {
            emits: [ { link_id: "cam", name: "video_stream" } ]
        },
        services: {
            exposes: [ { link_id: "cam", name: "video_stream_info" } ]
        }
    },
    execution: {
        language: "rust",
        run_cmd: ["sleep", "10"]
    }
}"#;

/// `node_add` must resolve the contract-backed entries and pass the
/// resolved topics/services to the generator so the staged peppygen nests
/// artifacts under `{category}/{contract_name}/{contract_tag}/`. Before the
/// fix, the working dir's `emitted_topics.rs` and
/// `exposed_services.rs` were empty for a node whose only contributions
/// came through an implemented contract.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_add_generates_contract_backed_modules_in_working_dir() {
    let started = start_core_node_with_mock_messenger().await;
    let node_stack = started.node_stack.clone();
    let peppy_dirs = started.peppy_dirs.clone();

    // Stage the contract on disk plus an fs-backed cache entry so
    // `resolve_implements` can find it.
    let contract_dir = TempDir::new().expect("contract tempdir");
    let contract_path = contract_dir.path().join("uvc_camera.json5");
    std::fs::write(&contract_path, CONTRACT_BODY).expect("write contract");

    TestPackagesCache::new()
        .contract_fs_entry("uvc_camera", "v1", &contract_path, CONTRACT_BODY)
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
        "node_add should resolve implements and succeed, got error: {:?}",
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
        "emitted_topics.rs should declare the implemented contract module \
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
        "exposed_services.rs should declare the implemented contract module \
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
