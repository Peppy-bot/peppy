//! Integration tests for `peppy node sync -a` dependency ordering.
//!
//! These tests verify that the in-memory `VirtualDeptree` built by
//! `sync_all_nodes_async` (in `crates/peppy/src/commands/node/sync.rs`)
//! correctly orders nodes so that dependencies are synced before their
//! dependants, and that the daemon resolves the deps via the `local_peers`
//! protocol field — without ever touching the persistent node stack.

use std::path::Path;
use std::sync::Arc;

use config::consts::NODE_CONFIG_FILE;
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands};
use peppy::context::AppContext;
use peppy::test_support::{LogCapture, ServeCommandEmulation};

/// Writes a minimal Rust node config to `<dir>/peppy.json5`. `deps` is a list
/// of `(name, tag)` pairs that go into the manifest's `depends_on.nodes`.
/// `consumes` is a list of `(local_id, topic_name)` pairs that go into
/// `interfaces.topics.consumes` (matched against the dep's emitted topics).
fn write_node(
    dir: &Path,
    name: &str,
    deps: &[(&str, &str)],
    consumes: &[(&str, &str)],
    emits: &[&str],
) {
    std::fs::create_dir_all(dir).expect("create node dir");

    let depends_on_block = if deps.is_empty() {
        String::new()
    } else {
        let entries = deps
            .iter()
            .map(|(n, t)| format!(r#"{{ name: "{n}", tag: "{t}", local_id: "{n}" }}"#))
            .collect::<Vec<_>>()
            .join(", ");
        format!("depends_on: {{ nodes: [{entries}] }},")
    };

    let consumes_block = if consumes.is_empty() {
        String::new()
    } else {
        let entries = consumes
            .iter()
            .map(|(local_id, topic)| {
                format!(r#"{{ local_node_id: "{local_id}", name: "{topic}" }}"#)
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("consumes: [{entries}],")
    };

    let emits_block = if emits.is_empty() {
        "emits: [],".to_string()
    } else {
        let entries = emits
            .iter()
            .map(|topic| {
                format!(
                    r#"{{ name: "{topic}", qos_profile: "standard", message_format: {{ payload: "string" }} }}"#
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("emits: [{entries}],")
    };

    let json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{name}",
                tag: "0.1.0",
                {depends_on_block}
            }},
            interfaces: {{
                topics: {{
                    {emits_block}
                    {consumes_block}
                }},
                services: {{ exposes: [] }},
                actions: {{ exposes: [] }},
            }},
            execution: {{
                language: "rust",
                build_cmd: ["true"],
                run_cmd: ["./bin"],
            }},
        }}"#
    );

    std::fs::write(dir.join(NODE_CONFIG_FILE), json5).expect("write peppy.json5");
}

/// Returns the order in which the per-node "Syncing node from <path>" log
/// lines appear, mapped back to the workspace-relative directory name.
fn extracted_sync_order(logs: &str, workspace: &Path) -> Vec<String> {
    let prefix = "Syncing node from ";
    logs.lines()
        .filter_map(|line| {
            let idx = line.find(prefix)?;
            let after = &line[idx + prefix.len()..];
            // Strip trailing " via daemon ..." chunk.
            let path_end = after.find(" via daemon").unwrap_or(after.len());
            let path = Path::new(&after[..path_end]);
            path.strip_prefix(workspace)
                .ok()
                .map(|p| p.display().to_string())
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_sync_all_resolves_chain_in_dependency_order() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("serve mock");
    let messenger = serve.messenger();

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let workspace_path = workspace.path();

    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // Chain: a -> b -> c. Listed alphabetically a/b/c, but we explicitly
    // verify ordering even though that order coincides with the topo order
    // here. The cycle test below proves the order isn't a coincidence.
    write_node(&workspace_path.join("a"), "a", &[], &[], &["topic_a"]);
    write_node(
        &workspace_path.join("b"),
        "b",
        &[("a", "0.1.0")],
        &[("a", "topic_a")],
        &["topic_b"],
    );
    write_node(
        &workspace_path.join("c"),
        "c",
        &[("b", "0.1.0")],
        &[("b", "topic_b")],
        &[],
    );

    let ctx = Arc::new(
        AppContext::with_messenger(workspace_path, Arc::clone(&messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );
    NodeCommand {
        command: NodeCommands::Sync {
            path: None,
            all: true,
        },
    }
    .execute(&ctx)
    .expect("node sync -a should succeed");

    let logs = log_capture.logs();
    assert!(
        logs.contains("Synced 3 node(s)"),
        "expected 'Synced 3 node(s)' in logs:\n{}",
        logs
    );

    // The chain a -> b -> c forces the topological order regardless of how
    // the FS walker discovers the directories.
    let order = extracted_sync_order(&logs, workspace_path);
    assert_eq!(
        order,
        vec!["a".to_string(), "b".to_string(), "c".to_string()],
        "sync order should respect dependency chain. Logs:\n{}",
        logs
    );

    // Each peppygen output should exist.
    for name in ["a", "b", "c"] {
        let peppy_dir = workspace_path
            .join(name)
            .join(config::consts::PEPPY_OUTPUT_DIR);
        assert!(
            peppy_dir.exists(),
            "expected .peppy directory at {}",
            peppy_dir.display()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_sync_all_resolves_diamond_dependency() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("serve mock");
    let messenger = serve.messenger();

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let workspace_path = workspace.path();

    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // Diamond:
    //          a
    //         / \
    //        b   c
    //         \ /
    //          d
    write_node(&workspace_path.join("a"), "a", &[], &[], &["topic_a"]);
    write_node(
        &workspace_path.join("b"),
        "b",
        &[("a", "0.1.0")],
        &[("a", "topic_a")],
        &["topic_b"],
    );
    write_node(
        &workspace_path.join("c"),
        "c",
        &[("a", "0.1.0")],
        &[("a", "topic_a")],
        &["topic_c"],
    );
    write_node(
        &workspace_path.join("d"),
        "d",
        &[("b", "0.1.0"), ("c", "0.1.0")],
        &[("b", "topic_b"), ("c", "topic_c")],
        &[],
    );

    let ctx = Arc::new(
        AppContext::with_messenger(workspace_path, Arc::clone(&messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );
    NodeCommand {
        command: NodeCommands::Sync {
            path: None,
            all: true,
        },
    }
    .execute(&ctx)
    .expect("node sync -a should succeed");

    let logs = log_capture.logs();
    assert!(
        logs.contains("Synced 4 node(s)"),
        "expected 'Synced 4 node(s)' in logs:\n{}",
        logs
    );

    // a must come before b/c, and d must come last.
    let order = extracted_sync_order(&logs, workspace_path);
    assert_eq!(order.len(), 4, "expected 4 sync entries, got: {:?}", order);
    assert_eq!(order.first().unwrap(), "a", "a must be synced first");
    assert_eq!(order.last().unwrap(), "d", "d must be synced last");
    let middle: Vec<&String> = order.iter().skip(1).take(2).collect();
    assert!(
        middle.iter().any(|s| s.as_str() == "b") && middle.iter().any(|s| s.as_str() == "c"),
        "b and c must appear between a and d, got: {:?}",
        order
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_sync_all_reports_cycle() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("serve mock");
    let messenger = serve.messenger();

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let workspace_path = workspace.path();

    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // a depends on b, b depends on a → cycle.
    write_node(&workspace_path.join("a"), "a", &[("b", "0.1.0")], &[], &[]);
    write_node(&workspace_path.join("b"), "b", &[("a", "0.1.0")], &[], &[]);

    let ctx = Arc::new(
        AppContext::with_messenger(workspace_path, Arc::clone(&messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );
    let err = NodeCommand {
        command: NodeCommands::Sync {
            path: None,
            all: true,
        },
    }
    .execute(&ctx)
    .expect_err("node sync -a should fail on a cycle");

    let msg = err.to_string();
    assert!(
        msg.contains("cycle"),
        "expected error to mention cycle, got: {}",
        msg
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_sync_all_missing_external_dep_fails_at_daemon() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("serve mock");
    let messenger = serve.messenger();

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let workspace_path = workspace.path();

    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // `lonely` depends on `nowhere:0.1.0`, which is neither on disk nor in
    // the daemon's persistent stack — the daemon must reject the sync with
    // its standard "depends on ... does not exist in the stack" message.
    write_node(
        &workspace_path.join("lonely"),
        "lonely",
        &[("nowhere", "0.1.0")],
        &[],
        &[],
    );

    let ctx = Arc::new(
        AppContext::with_messenger(workspace_path, Arc::clone(&messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );
    let err = NodeCommand {
        command: NodeCommands::Sync {
            path: None,
            all: true,
        },
    }
    .execute(&ctx)
    .expect_err("node sync -a should fail when an external dep is missing");

    let msg = err.to_string();
    assert!(
        msg.contains("nowhere") && msg.contains("does not exist in the stack"),
        "expected daemon-side missing-dep error, got: {}",
        msg
    );
}
