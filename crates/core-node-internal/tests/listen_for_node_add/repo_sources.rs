use super::*;
use common::{TestPackagesCache, write_plain_peppy_json5};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_node_add_single_no_deps_fs_source() {
    let started = start_core_node_with_mock_messenger().await;
    let node_stack = started.node_stack.clone();
    let peppy_dirs = started.peppy_dirs.clone();

    let tmp = TempDir::new().unwrap();
    let node_dir = tmp.path().join("standalone");
    write_plain_peppy_json5(&node_dir, &minimal_node_config("standalone", "0.1.0", &[]));

    TestPackagesCache::new()
        .fs_entry("standalone", "0.1.0", &node_dir, &[])
        .write(&peppy_dirs);

    let res = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::RepoNode {
            name: "standalone",
            tag: "0.1.0",
            variant: "default",
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
    assert!(node_stack.contains("standalone", "0.1.0", "default"));
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
    write_plain_peppy_json5(&a_dir, &minimal_node_config("a", "0.1.0", &[]));
    write_plain_peppy_json5(
        &b_dir,
        &minimal_node_config("b", "0.1.0", &[("a", "0.1.0")]),
    );
    write_plain_peppy_json5(
        &c_dir,
        &minimal_node_config("c", "0.1.0", &[("b", "0.1.0")]),
    );

    TestPackagesCache::new()
        .fs_entry("a", "0.1.0", &a_dir, &[])
        .fs_entry("b", "0.1.0", &b_dir, &[])
        .fs_entry("c", "0.1.0", &c_dir, &[])
        .write(&peppy_dirs);

    let res = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::RepoNode {
            name: "c",
            tag: "0.1.0",
            variant: "default",
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
    assert!(node_stack.contains("a", "0.1.0", "default"));
    assert!(node_stack.contains("b", "0.1.0", "default"));
    assert!(node_stack.contains("c", "0.1.0", "default"));
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
    write_plain_peppy_json5(&a, &minimal_node_config("a", "0.1.0", &[]));
    write_plain_peppy_json5(&b, &minimal_node_config("b", "0.1.0", &[("a", "0.1.0")]));
    write_plain_peppy_json5(&c, &minimal_node_config("c", "0.1.0", &[("a", "0.1.0")]));
    write_plain_peppy_json5(
        &d,
        &minimal_node_config("d", "0.1.0", &[("b", "0.1.0"), ("c", "0.1.0")]),
    );

    TestPackagesCache::new()
        .fs_entry("a", "0.1.0", &a, &[])
        .fs_entry("b", "0.1.0", &b, &[])
        .fs_entry("c", "0.1.0", &c, &[])
        .fs_entry("d", "0.1.0", &d, &[])
        .write(&peppy_dirs);

    let res = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::RepoNode {
            name: "d",
            tag: "0.1.0",
            variant: "default",
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
        assert!(
            node_stack.contains(name, "0.1.0", "default"),
            "missing {name}"
        );
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
    write_plain_peppy_json5(&b_dir, &minimal_node_config("b", "0.1.0", &[]));
    write_plain_peppy_json5(&c_dir, &minimal_node_config("c", "0.1.0", &[]));
    write_plain_peppy_json5(
        &a_dir,
        &minimal_node_config("a", "0.1.0", &[("b", "0.1.0"), ("c", "0.1.0")]),
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
        .fs_entry("a", "0.1.0", &a_dir, &[])
        .fs_entry("b", "0.1.0", &b_dir, &[])
        .fs_entry("c", "0.1.0", &c_dir, &[])
        .write(&peppy_dirs);

    let res = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::RepoNode {
            name: "a",
            tag: "0.1.0",
            variant: "default",
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
    assert!(node_stack.contains("a", "0.1.0", "default"));
    assert!(node_stack.contains("b", "0.1.0", "default"));
    assert!(node_stack.contains("c", "0.1.0", "default"));
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
    write_plain_peppy_json5(&c_dir, &minimal_node_config("c", "0.1.0", &[]));

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
    write_plain_peppy_json5(
        &b_dir,
        &minimal_node_config("b", "0.1.0", &[("c", "0.1.0")]),
    );
    let a_dir = tmp.path().join("a");
    write_plain_peppy_json5(
        &a_dir,
        &minimal_node_config("a", "0.1.0", &[("b", "0.1.0")]),
    );

    TestPackagesCache::new()
        .fs_entry("a", "0.1.0", &a_dir, &[])
        .fs_entry("b", "0.1.0", &b_dir, &[])
        // NB: intentionally no entry for `d` — a correct resolver only
        // walks deps declared by the resolved manifests, and none of them
        // mention `d`.
        .fs_entry("c", "0.1.0", &c_dir, &[])
        .write(&peppy_dirs);

    let res = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::RepoNode {
            name: "a",
            tag: "0.1.0",
            variant: "default",
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
    assert!(node_stack.contains("a", "0.1.0", "default"));
    assert!(node_stack.contains("b", "0.1.0", "default"));
    assert!(node_stack.contains("c", "0.1.0", "default"));
    assert!(
        !node_stack.contains("d", "0.1.0", "default"),
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
    write_plain_peppy_json5(&b_dir, &minimal_node_config("b", "0.1.0", &[]));
    write_plain_peppy_json5(&e_dir, &minimal_node_config("e", "0.1.0", &[]));

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
    write_plain_peppy_json5(&d_dir, &minimal_node_config("d", "0.1.0", &[]));
    write_plain_peppy_json5(
        &c_dir,
        &minimal_node_config("c", "0.1.0", &[("d", "0.1.0"), ("e", "0.1.0")]),
    );
    write_plain_peppy_json5(
        &a_dir,
        &minimal_node_config("a", "0.1.0", &[("b", "0.1.0"), ("c", "0.1.0")]),
    );

    TestPackagesCache::new()
        .fs_entry("a", "0.1.0", &a_dir, &[])
        .fs_entry("b", "0.1.0", &b_dir, &[])
        .fs_entry("c", "0.1.0", &c_dir, &[])
        .fs_entry("d", "0.1.0", &d_dir, &[])
        .fs_entry("e", "0.1.0", &e_dir, &[])
        .write(&peppy_dirs);

    let res = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::RepoNode {
            name: "a",
            tag: "0.1.0",
            variant: "default",
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
        assert!(
            node_stack.contains(name, "0.1.0", "default"),
            "missing {name}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_node_add_root_variant_only() {
    let started = start_core_node_with_mock_messenger().await;
    let node_stack = started.node_stack.clone();
    let peppy_dirs = started.peppy_dirs.clone();

    // Root uses the `default` variant idiom — no execution at root, all
    // execution comes from the variant. The user selects the non-default
    // variant via `--variant <name>`.
    let tmp = TempDir::new().unwrap();
    let node_dir = tmp.path().join("with_variants");
    let default_dir = node_dir.join("default");
    let alt_dir = node_dir.join("alt");
    std::fs::create_dir_all(&default_dir).unwrap();
    std::fs::create_dir_all(&alt_dir).unwrap();

    let root_peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "with_variants",
                tag: "0.1.0",
                variants: [
                    { name: "default", source: { local: "./default" } },
                    { name: "alt", source: { local: "./alt" } }
                ]
            },
            interfaces: {}
        }"#;
    write_plain_peppy_json5(&node_dir, root_peppy_json5);

    for d in [&default_dir, &alt_dir] {
        std::fs::write(
            d.join(NODE_CONFIG_FILE),
            r#"{
                peppy_schema: "node_v1",
                execution: {
                    language: "rust",
                    run_cmd: ["sleep", "10"]
                }
            }"#,
        )
        .unwrap();
    }

    TestPackagesCache::new()
        .fs_entry("with_variants", "0.1.0", &node_dir, &["default", "alt"])
        .write(&peppy_dirs);

    let res = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::RepoNode {
            name: "with_variants",
            tag: "0.1.0",
            variant: "alt",
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
    assert!(node_stack.contains("with_variants", "0.1.0", "alt"));
}

// TODO: re-express via manifest-declared `NodeDependency.variant` instead of
// the deleted `DepVariantOverride` plumbing. See repo_sources.rs:466.
#[ignore = "DepVariantOverride deleted; rewrite via manifest variant field"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_node_add_dep_variants_only_no_root() {}

// TODO: re-express via manifest-declared `NodeDependency.variant`. See repo_sources.rs:471.
#[ignore = "DepVariantOverride deleted; rewrite via manifest variant field"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_node_add_dep_override_on_unrelated_node_warns() {}

// TODO: re-express via manifest-declared `NodeDependency.variant`. See repo_sources.rs:476.
#[ignore = "DepVariantOverride deleted; rewrite via manifest variant field"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_node_add_re_add_swaps_dep_variant() {}
