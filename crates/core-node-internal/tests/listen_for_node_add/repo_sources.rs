use super::*;
use common::{TestPackagesCache, write_plain_peppy_json5};

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

    let res = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::RepoNode {
            name: "standalone",
            tag: "v1",
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add should complete");

    assert!(
        res.success,
        "add should succeed, err={:?}",
        res.error_message
    );
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

    let res = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::RepoNode {
            name: "c",
            tag: "v1",
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add should complete");

    assert!(
        res.success,
        "add should succeed, err={:?}",
        res.error_message
    );
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

    let res = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::RepoNode {
            name: "d",
            tag: "v1",
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add should complete");

    assert!(
        res.success,
        "add should succeed, err={:?}",
        res.error_message
    );
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
    let pre = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        b_dir.as_path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("pre-add for b should complete");
    assert!(pre.success, "pre-add for b should succeed");
    assert_eq!(node_stack.len(), 2, "root + b");

    TestPackagesCache::new()
        .fs_entry("a", "v1", &a_dir)
        .fs_entry("b", "v1", &b_dir)
        .fs_entry("c", "v1", &c_dir)
        .write(&peppy_dirs);

    let res = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::RepoNode {
            name: "a",
            tag: "v1",
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add should complete");

    assert!(
        res.success,
        "add should succeed, err={:?}",
        res.error_message
    );
    // Stack: root + b + c + a = 4. B's stack entry is replaced in-place
    // by the fresh materialization (same key, same count), and c + a are
    // added as new entries.
    assert!(node_stack.contains("a", "v1"));
    assert!(node_stack.contains("b", "v1"));
    assert!(node_stack.contains("c", "v1"));
    assert_eq!(node_stack.len(), 4);
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
    let pre = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        c_dir.as_path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("pre-add for c should complete");
    assert!(pre.success);
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
        // NB: intentionally no entry for `d` — a correct resolver only
        // walks deps declared by the resolved manifests, and none of them
        // mention `d`.
        .fs_entry("c", "v1", &c_dir)
        .write(&peppy_dirs);

    let res = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::RepoNode {
            name: "a",
            tag: "v1",
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add should complete");

    assert!(
        res.success,
        "add should succeed, err={:?}",
        res.error_message
    );
    // Stack = root + c + b + a = 4. c is replaced in-place when re-
    // materialized; b and a are new.
    assert_eq!(node_stack.len(), 4);
    assert!(node_stack.contains("a", "v1"));
    assert!(node_stack.contains("b", "v1"));
    assert!(node_stack.contains("c", "v1"));
    assert!(
        !node_stack.contains("d", "v1"),
        "d must never be added — no manifest in the tree declares it"
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
    //   - b and e keep their keys but are replaced in-place by fresh
    //     materializations (push_config_impl's same-key update path).
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
        send_node_add_and_wait(
            &started.caller_handle,
            &started.core_node_name,
            dir.as_path(),
            GOAL_TIMEOUT,
            RESULT_TIMEOUT,
            None,
        )
        .await
        .expect("pre-add should complete")
        .success
        .then_some(())
        .expect("pre-add should succeed");
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

    let res = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::RepoNode {
            name: "a",
            tag: "v1",
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add should complete");

    assert!(
        res.success,
        "add should succeed, err={:?}",
        res.error_message
    );
    assert_eq!(
        node_stack.len(),
        6,
        "root + b + e (pre-populated) + d + c + a (newly added)"
    );
    for name in ["a", "b", "c", "d", "e"] {
        assert!(node_stack.contains(name, "v1"), "missing {name}");
    }
}
