use super::*;
use common::{
    TestPackagesCache, send_node_add_and_wait_with_dep_overrides, write_plain_peppy_json5,
};
use core_node::encoding::DepVariantOverride;

fn minimal_node_config(name: &str, tag: &str, deps: &[(&str, &str)]) -> String {
    let depends_on = if deps.is_empty() {
        String::new()
    } else {
        let nodes = deps
            .iter()
            .map(|(n, t)| format!(r#"{{ name: "{n}", tag: "{t}", local_id: "{n}" }}"#))
            .collect::<Vec<_>>()
            .join(", ");
        format!(r#"depends_on: {{ nodes: [{nodes}] }},"#)
    };
    format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{name}",
                tag: "{tag}",
                {depends_on}
            }},
            execution: {{
                language: "rust",
                run_cmd: ["sleep", "10"]
            }}
        }}"#
    )
}

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
    assert!(node_stack.contains("standalone", "0.1.0"));
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
    assert!(node_stack.contains("a", "0.1.0"));
    assert!(node_stack.contains("b", "0.1.0"));
    assert!(node_stack.contains("c", "0.1.0"));
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
        assert!(node_stack.contains(name, "0.1.0"), "missing {name}");
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
    // Stack now has: root + b (pre-existing) + c (added) + a (added) = 4.
    // B should still be there (reused), C and A should be newly added.
    assert!(node_stack.contains("a", "0.1.0"));
    assert!(node_stack.contains("b", "0.1.0"));
    assert!(node_stack.contains("c", "0.1.0"));
    assert_eq!(node_stack.len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_node_add_mixed_tree_stops_recursion_at_stack_dep() {
    // Tree: a (cache) → b (cache) → c (stack) → d (cache, never visited).
    //
    // When we hit `c` during resolution, we find it already in the stack
    // and skip it. Its declared dep `d` must therefore NOT be materialized:
    // the stack entry for `c` is authoritative, so the rest of its subtree
    // is opaque to this batch.
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

    // `d` is declared in the cache but MUST NOT be looked at because `c`
    // is in the stack. Deliberately *omit* `d` from the cache to prove
    // the resolver short-circuits at stack deps rather than pursuing the
    // rest of the declared tree.
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
        // NB: intentionally no entry for `d` — a correct resolver must
        // never ask for it because `c` in the stack cuts recursion short.
        .fs_entry("c", "0.1.0", &c_dir, &[])
        .write(&peppy_dirs);

    let res = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::RepoNode {
            name: "a",
            tag: "0.1.0",
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
    // Stack = root + c (pre-existing) + b (added) + a (added).
    assert_eq!(node_stack.len(), 4);
    assert!(node_stack.contains("a", "0.1.0"));
    assert!(node_stack.contains("b", "0.1.0"));
    assert!(node_stack.contains("c", "0.1.0"));
    assert!(
        !node_stack.contains("d", "0.1.0"),
        "d must not be added — its parent c was served from the stack"
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
    //   - b and e stay as-is (pre-populated stack entries, skipped).
    //   - a, c, d get materialized from the cache and added in topo order.
    //   - Stack size: root + b + e (pre) + d + c + a (added) = 6.
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
        assert!(node_stack.contains(name, "0.1.0"), "missing {name}");
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
            schema_version: 1,
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
                schema_version: 1,
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

    let res = send_node_add_and_wait_with_dep_overrides(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::RepoNode {
            name: "with_variants",
            tag: "0.1.0",
        },
        Some("alt"),
        Vec::new(),
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
    assert!(node_stack.contains("with_variants", "0.1.0"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_node_add_dep_variants_only_no_root() {
    let started = start_core_node_with_mock_messenger().await;
    let node_stack = started.node_stack.clone();
    let peppy_dirs = started.peppy_dirs.clone();

    // Dep uses the `default` variant idiom with an extra `mock` variant.
    let tmp = TempDir::new().unwrap();
    let dep_dir = tmp.path().join("dep_with_variant");
    let default_dir = dep_dir.join("default");
    let mock_dir = dep_dir.join("mock");
    std::fs::create_dir_all(&default_dir).unwrap();
    std::fs::create_dir_all(&mock_dir).unwrap();
    let dep_peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "dep_with_variant",
                tag: "0.1.0",
                variants: [
                    { name: "default", source: { local: "./default" } },
                    { name: "mock", source: { local: "./mock" } }
                ]
            },
            interfaces: {}
        }"#;
    write_plain_peppy_json5(&dep_dir, dep_peppy_json5);
    for d in [&default_dir, &mock_dir] {
        std::fs::write(
            d.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                execution: {
                    language: "rust",
                    run_cmd: ["sleep", "10"]
                }
            }"#,
        )
        .unwrap();
    }

    let target_dir = tmp.path().join("target");
    write_plain_peppy_json5(
        &target_dir,
        &minimal_node_config("target", "1.0.0", &[("dep_with_variant", "0.1.0")]),
    );

    TestPackagesCache::new()
        .fs_entry("dep_with_variant", "0.1.0", &dep_dir, &["mock"])
        .fs_entry("target", "1.0.0", &target_dir, &[])
        .write(&peppy_dirs);

    let res = send_node_add_and_wait_with_dep_overrides(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::RepoNode {
            name: "target",
            tag: "1.0.0",
        },
        None,
        vec![DepVariantOverride {
            name: "dep_with_variant".into(),
            tag: "0.1.0".into(),
            variant: "mock".into(),
        }],
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
    assert!(node_stack.contains("dep_with_variant", "0.1.0"));
    assert!(node_stack.contains("target", "1.0.0"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_node_add_dep_override_on_unrelated_node_warns() {
    let started = start_core_node_with_mock_messenger().await;
    let node_stack = started.node_stack.clone();
    let peppy_dirs = started.peppy_dirs.clone();

    let tmp = TempDir::new().unwrap();
    let target_dir = tmp.path().join("target");
    write_plain_peppy_json5(&target_dir, &minimal_node_config("target", "1.0.0", &[]));

    TestPackagesCache::new()
        .fs_entry("target", "1.0.0", &target_dir, &[])
        .write(&peppy_dirs);

    let (fb_tx, mut fb_rx) = tokio::sync::mpsc::unbounded_channel::<NodeAddFeedback>();
    let res = send_node_add_and_wait_with_dep_overrides(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::RepoNode {
            name: "target",
            tag: "1.0.0",
        },
        None,
        vec![DepVariantOverride {
            name: "nonexistent".into(),
            tag: "9.9.9".into(),
            variant: "whatever".into(),
        }],
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        Some(fb_tx),
    )
    .await
    .expect("node_add should complete");

    assert!(
        res.success,
        "add should succeed despite orphan override, err={:?}",
        res.error_message
    );
    assert!(node_stack.contains("target", "1.0.0"));

    let mut feedback = Vec::new();
    while let Ok(f) = fb_rx.try_recv() {
        feedback.push(f);
    }
    let warned = feedback.iter().any(|f| {
        f.is_warning() && f.line.contains("nonexistent:9.9.9") && f.line.contains("ignored")
    });
    assert!(
        warned,
        "expected a warning feedback line about the orphan override; got: {feedback:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_node_add_dep_already_in_stack_keeps_its_variant() {
    let started = start_core_node_with_mock_messenger().await;
    let node_stack = started.node_stack.clone();
    let peppy_dirs = started.peppy_dirs.clone();

    let tmp = TempDir::new().unwrap();
    let dep_dir = tmp.path().join("dep");
    write_plain_peppy_json5(&dep_dir, &minimal_node_config("dep", "0.1.0", &[]));

    // Pre-add dep directly (no variant).
    send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        dep_dir.as_path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("pre-add of dep should complete")
    .success
    .then_some(())
    .expect("pre-add of dep should succeed");

    let target_dir = tmp.path().join("target");
    write_plain_peppy_json5(
        &target_dir,
        &minimal_node_config("target", "1.0.0", &[("dep", "0.1.0")]),
    );

    TestPackagesCache::new()
        .fs_entry("dep", "0.1.0", &dep_dir, &["mock"])
        .fs_entry("target", "1.0.0", &target_dir, &[])
        .write(&peppy_dirs);

    let (fb_tx, mut fb_rx) = tokio::sync::mpsc::unbounded_channel::<NodeAddFeedback>();
    let res = send_node_add_and_wait_with_dep_overrides(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::RepoNode {
            name: "target",
            tag: "1.0.0",
        },
        None,
        vec![DepVariantOverride {
            name: "dep".into(),
            tag: "0.1.0".into(),
            variant: "mock".into(),
        }],
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        Some(fb_tx),
    )
    .await
    .expect("node_add should complete");

    assert!(
        res.success,
        "add should succeed, err={:?}",
        res.error_message
    );
    // Stack still contains the pre-existing dep (unchanged) + target.
    assert!(node_stack.contains("dep", "0.1.0"));
    assert!(node_stack.contains("target", "1.0.0"));

    let mut feedback = Vec::new();
    while let Ok(f) = fb_rx.try_recv() {
        feedback.push(f);
    }
    let warned = feedback.iter().any(|f| {
        f.is_warning() && f.line.contains("dep:0.1.0") && f.line.contains("already in stack")
    });
    assert!(
        warned,
        "expected a warning about override ignored for already-in-stack dep; got: {feedback:?}"
    );
}
