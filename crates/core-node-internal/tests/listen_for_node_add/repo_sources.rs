use super::*;
use common::{TestPackagesCache, write_plain_peppy_json5};
use core_node_api::encoding::NodeAddResult;

/// Adds `source` and asserts that the add succeeded, naming the add `what`
/// in the failure messages. Returns the result of the add.
async fn add_node_successfully<'a>(
    started: &common::StartedCoreNode,
    source: impl Into<NodeAddSource<'a>>,
    what: &str,
) -> NodeAddResult {
    let result = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        source,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .unwrap_or_else(|err| panic!("{what} should complete: {err}"));
    assert!(
        result.success,
        "{what} should succeed, err={:?}",
        result.error_message
    );
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_node_add_single_no_deps_fs_source() {
    let started = start_core_node_with_mock_messenger().await;
    let node_stack = started.node_stack.clone();
    let peppy_dirs = started.peppy_dirs.clone();

    let tmp = TempDir::new().unwrap();
    let node_dir = tmp.path().join("standalone");
    write_plain_peppy_json5(&node_dir, &minimal_node_config("standalone", "v1", &[]));

    TestPackagesCache::new()
        .fs_entry("standalone", "v1", &node_dir)
        .write(&peppy_dirs);

    add_node_successfully(
        &started,
        NodeAddSource::ResolveRef {
            name: "standalone",
            tag: "v1",
        },
        "add",
    )
    .await;
    assert!(node_stack.contains("standalone", "v1"));
    assert_eq!(node_stack.len(), 2, "root + standalone");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_node_add_chain_of_three_fs() {
    let started = start_core_node_with_mock_messenger().await;
    let node_stack = started.node_stack.clone();
    let peppy_dirs = started.peppy_dirs.clone();

    let tmp = TempDir::new().unwrap();
    let a_dir = tmp.path().join("a");
    let b_dir = tmp.path().join("b");
    let c_dir = tmp.path().join("c");
    write_plain_peppy_json5(&a_dir, &minimal_node_config("a", "v1", &[]));
    write_plain_peppy_json5(&b_dir, &minimal_node_config("b", "v1", &[("a", "v1")]));
    write_plain_peppy_json5(&c_dir, &minimal_node_config("c", "v1", &[("b", "v1")]));

    TestPackagesCache::new()
        .fs_entry("a", "v1", &a_dir)
        .fs_entry("b", "v1", &b_dir)
        .fs_entry("c", "v1", &c_dir)
        .write(&peppy_dirs);

    let res = add_node_successfully(
        &started,
        NodeAddSource::ResolveRef {
            name: "c",
            tag: "v1",
        },
        "add",
    )
    .await;
    assert!(node_stack.contains("a", "v1"));
    assert!(node_stack.contains("b", "v1"));
    assert!(node_stack.contains("c", "v1"));
    assert_eq!(node_stack.len(), 4, "root + a + b + c");
    assert_eq!(
        res.node_name.as_deref(),
        Some("c"),
        "result should report the root target name"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_node_add_diamond() {
    let started = start_core_node_with_mock_messenger().await;
    let node_stack = started.node_stack.clone();
    let peppy_dirs = started.peppy_dirs.clone();

    let tmp = TempDir::new().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    let c = tmp.path().join("c");
    let d = tmp.path().join("d");
    write_plain_peppy_json5(&a, &minimal_node_config("a", "v1", &[]));
    write_plain_peppy_json5(&b, &minimal_node_config("b", "v1", &[("a", "v1")]));
    write_plain_peppy_json5(&c, &minimal_node_config("c", "v1", &[("a", "v1")]));
    write_plain_peppy_json5(
        &d,
        &minimal_node_config("d", "v1", &[("b", "v1"), ("c", "v1")]),
    );

    TestPackagesCache::new()
        .fs_entry("a", "v1", &a)
        .fs_entry("b", "v1", &b)
        .fs_entry("c", "v1", &c)
        .fs_entry("d", "v1", &d)
        .write(&peppy_dirs);

    add_node_successfully(
        &started,
        NodeAddSource::ResolveRef {
            name: "d",
            tag: "v1",
        },
        "add",
    )
    .await;
    for name in ["a", "b", "c", "d"] {
        assert!(node_stack.contains(name, "v1"), "missing {name}");
    }
    assert_eq!(
        node_stack.len(),
        5,
        "root + a + b + c + d (single entry for a)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_node_add_partial_stack_coverage() {
    let started = start_core_node_with_mock_messenger().await;
    let node_stack = started.node_stack.clone();
    let peppy_dirs = started.peppy_dirs.clone();

    let tmp = TempDir::new().unwrap();
    let b_dir = tmp.path().join("b");
    let c_dir = tmp.path().join("c");
    let a_dir = tmp.path().join("a");
    write_plain_peppy_json5(&b_dir, &minimal_node_config("b", "v1", &[]));
    write_plain_peppy_json5(&c_dir, &minimal_node_config("c", "v1", &[]));
    write_plain_peppy_json5(
        &a_dir,
        &minimal_node_config("a", "v1", &[("b", "v1"), ("c", "v1")]),
    );

    // Pre-populate the stack with dep B by adding it directly first.
    add_node_successfully(&started, b_dir.as_path(), "pre-add for b").await;
    assert_eq!(node_stack.len(), 2, "root + b");

    TestPackagesCache::new()
        .fs_entry("a", "v1", &a_dir)
        .fs_entry("b", "v1", &b_dir)
        .fs_entry("c", "v1", &c_dir)
        .write(&peppy_dirs);

    add_node_successfully(
        &started,
        NodeAddSource::ResolveRef {
            name: "a",
            tag: "v1",
        },
        "add",
    )
    .await;
    // Stack: root + b + c + a = 4. B's manifest is unchanged, so its
    // stack entry is left alone (same key, same count), and c + a are
    // added as new entries.
    assert!(node_stack.contains("a", "v1"));
    assert!(node_stack.contains("b", "v1"));
    assert!(node_stack.contains("c", "v1"));
    assert_eq!(node_stack.len(), 4);
}

/// The launch-shaped sequence: a dependency is added and BUILT on its own,
/// then a dependent's batch resolves the same identity from the cache with
/// an unchanged manifest. The batch must leave the built entity alone:
/// re-pushing it would reset it to `Added` and silently discard the
/// artifact, so a stack launch whose later deployment depends on an
/// earlier, already-built one would fail its run phase with "added but
/// not built".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_node_add_preserves_built_unchanged_dependency() {
    let started = start_core_node_with_mock_messenger().await;
    let node_stack = started.node_stack.clone();
    let peppy_dirs = started.peppy_dirs.clone();

    let tmp = TempDir::new().unwrap();
    let dep_dir = tmp.path().join("dep");
    let top_dir = tmp.path().join("top");
    write_plain_peppy_json5(&dep_dir, &minimal_node_config("dep", "v1", &[]));
    write_plain_peppy_json5(
        &top_dir,
        &minimal_node_config("top", "v1", &[("dep", "v1")]),
    );

    TestPackagesCache::new()
        .fs_entry("dep", "v1", &dep_dir)
        .fs_entry("top", "v1", &top_dir)
        .write(&peppy_dirs);

    add_node_successfully(
        &started,
        NodeAddSource::ResolveRef {
            name: "dep",
            tag: "v1",
        },
        "pre-add for dep",
    )
    .await;
    build_staged_node(&started, "dep", "v1").await;
    let built = entity_artifact_path(&node_stack, "dep", "v1");

    add_node_successfully(
        &started,
        NodeAddSource::ResolveRef {
            name: "top",
            tag: "v1",
        },
        "add",
    )
    .await;

    assert!(node_stack.contains("top", "v1"));
    // `entity_artifact_path` panics if the entity was reset to `Added`,
    // which is exactly the regression this guards against.
    assert_eq!(
        entity_artifact_path(&node_stack, "dep", "v1"),
        built,
        "an unchanged, already-built dependency must keep its artifact through a batch add"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_node_add_does_not_materialize_nodes_outside_declared_tree() {
    // Tree: a (cache) → b (cache) → c (pre-existing in stack, no deps).
    //
    // `c`'s manifest declares no dependencies, so the resolver must not
    // fetch any unrelated node from the cache. The test omits a fictional
    // `d` from the cache to prove the resolver only walks what manifests
    // declare, rather than speculatively pulling entries.
    let started = start_core_node_with_mock_messenger().await;
    let node_stack = started.node_stack.clone();
    let peppy_dirs = started.peppy_dirs.clone();

    let tmp = TempDir::new().unwrap();
    let c_dir = tmp.path().join("c");
    write_plain_peppy_json5(&c_dir, &minimal_node_config("c", "v1", &[]));

    // Pre-add c directly so it's already in the stack.
    add_node_successfully(&started, c_dir.as_path(), "pre-add for c").await;
    assert_eq!(node_stack.len(), 2, "root + c");

    // Deliberately omit a hypothetical `d` from the cache: no manifest
    // references `d`, so the resolver must never look it up. If it did,
    // the batch would fail with "Dependencies missing from packages cache".
    let b_dir = tmp.path().join("b");
    write_plain_peppy_json5(&b_dir, &minimal_node_config("b", "v1", &[("c", "v1")]));
    let a_dir = tmp.path().join("a");
    write_plain_peppy_json5(&a_dir, &minimal_node_config("a", "v1", &[("b", "v1")]));

    TestPackagesCache::new()
        .fs_entry("a", "v1", &a_dir)
        .fs_entry("b", "v1", &b_dir)
        // NB: intentionally no entry for `d`; a correct resolver only
        // walks deps declared by the resolved manifests, and none of them
        // mention `d`.
        .fs_entry("c", "v1", &c_dir)
        .write(&peppy_dirs);

    add_node_successfully(
        &started,
        NodeAddSource::ResolveRef {
            name: "a",
            tag: "v1",
        },
        "add",
    )
    .await;
    // Stack = root + c + b + a = 4. c is re-materialized (to walk its
    // deps) but left in place, its manifest being unchanged; b and a are
    // new.
    assert_eq!(node_stack.len(), 4);
    assert!(node_stack.contains("a", "v1"));
    assert!(node_stack.contains("b", "v1"));
    assert!(node_stack.contains("c", "v1"));
    assert!(
        !node_stack.contains("d", "v1"),
        "d must never be added; no manifest in the tree declares it"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_node_add_deep_mixed_tree_stack_and_cache_at_multiple_levels() {
    // Tree (dependency direction = arrow points at provider):
    //
    //              a (cache, target)
    //             /                \
    //          b (stack)         c (cache)
    //                            /       \
    //                         d (cache)  e (stack)
    //
    // Expected outcome:
    //   - b and e keep their keys; their manifests are unchanged, so their
    //     stack entries are left alone rather than re-pushed.
    //   - a, c, d are materialized from the cache and pushed in topo order.
    //   - Stack size: root + b + e + d + c + a = 6.
    let started = start_core_node_with_mock_messenger().await;
    let node_stack = started.node_stack.clone();
    let peppy_dirs = started.peppy_dirs.clone();

    let tmp = TempDir::new().unwrap();
    let b_dir = tmp.path().join("b");
    let e_dir = tmp.path().join("e");
    write_plain_peppy_json5(&b_dir, &minimal_node_config("b", "v1", &[]));
    write_plain_peppy_json5(&e_dir, &minimal_node_config("e", "v1", &[]));

    // Pre-populate stack with b and e.
    for dir in [&b_dir, &e_dir] {
        add_node_successfully(&started, dir.as_path(), "pre-add").await;
    }
    assert_eq!(node_stack.len(), 3, "root + b + e");

    let d_dir = tmp.path().join("d");
    let c_dir = tmp.path().join("c");
    let a_dir = tmp.path().join("a");
    write_plain_peppy_json5(&d_dir, &minimal_node_config("d", "v1", &[]));
    write_plain_peppy_json5(
        &c_dir,
        &minimal_node_config("c", "v1", &[("d", "v1"), ("e", "v1")]),
    );
    write_plain_peppy_json5(
        &a_dir,
        &minimal_node_config("a", "v1", &[("b", "v1"), ("c", "v1")]),
    );

    TestPackagesCache::new()
        .fs_entry("a", "v1", &a_dir)
        .fs_entry("b", "v1", &b_dir)
        .fs_entry("c", "v1", &c_dir)
        .fs_entry("d", "v1", &d_dir)
        .fs_entry("e", "v1", &e_dir)
        .write(&peppy_dirs);

    add_node_successfully(
        &started,
        NodeAddSource::ResolveRef {
            name: "a",
            tag: "v1",
        },
        "add",
    )
    .await;
    assert_eq!(
        node_stack.len(),
        6,
        "root + b + e (pre-populated) + d + c + a (newly added)"
    );
    for name in ["a", "b", "c", "d", "e"] {
        assert!(node_stack.contains(name, "v1"), "missing {name}");
    }
}
