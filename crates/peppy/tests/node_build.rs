//! `peppy node build` against the build artifact cache: a build of sources
//! byte-identical to an earlier build's reuses that build's artifact, a
//! changed tree builds again, `--rebuild` bypasses a hit, and the cache
//! outlives the daemon that filled it.
//!
//! `peppy node build` only accepts an `Added` entity, so "build twice" is
//! always add, build, add, build. Feedback lines are not observable through
//! `LogCapture`, so every assertion reads the daemon's build log on disk and
//! the counter file the `build_cmd` appends to.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use config::node::Toolchain;
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands, NodeName};
use peppy::context::AppContext;
use peppy::test_support::{CACHED_BUILD_REUSE_PREFIX, ServeCommandEmulation, override_build_cmd};
use tempfile::TempDir;

use super::common::{build_count, build_logs_for, built_artifacts_for, counting_build_cmd};

const NODE_TAG: &str = "v1";

/// A Cargo-scaffolded node whose `build_cmd` counts its runs in a file
/// outside the node directory.
struct CountedNode {
    name: &'static str,
    path: PathBuf,
    counter: PathBuf,
    _node_root: TempDir,
    _control: TempDir,
}

impl CountedNode {
    fn scaffold(ctx: &Arc<AppContext>, name: &'static str, toolchain: Toolchain) -> Self {
        let node_root = TempDir::new().expect("node root tempdir");
        let control = TempDir::new().expect("control tempdir");
        let counter = control.path().join("builds");

        NodeCommand {
            command: NodeCommands::Init {
                node_name: NodeName::new(name).expect("valid node name"),
                to_dir: Some(node_root.path().to_path_buf()),
                toolchain,
                with_container: false,
            },
        }
        .execute(ctx)
        .expect("node init should succeed");

        let path = node_root.path().join(name);
        override_build_cmd(&path.join("peppy.json5"), counting_build_cmd(&counter));
        Self {
            name,
            path,
            counter,
            _node_root: node_root,
            _control: control,
        }
    }

    /// Stages the node and builds it, the two commands `peppy node build`
    /// needs in that order.
    fn add_and_build(&self, ctx: &Arc<AppContext>, rebuild: bool) {
        NodeCommand {
            command: NodeCommands::Add {
                source: Some(self.path.display().to_string()),
                git_ref: None,
                sync: false,
                build: false,
                run: false,
                args: Vec::new(),
                instance_id: None,
                links: Vec::new(),
                vacant_links: Vec::new(),
                idle_timeout: 60,
                max_timeout: 3600,
                force: false,
            },
        }
        .execute(ctx)
        .expect("node add should succeed");
        NodeCommand {
            command: NodeCommands::Build {
                node_ref: (self.name.to_string(), NODE_TAG.to_string()),
                idle_timeout: 60,
                max_timeout: 3600,
                force: false,
                rebuild,
            },
        }
        .execute(ctx)
        .expect("node build should succeed");
    }

    fn builds(&self) -> usize {
        build_count(&self.counter)
    }

    fn artifacts(&self, peppy_root: &Path) -> Vec<PathBuf> {
        built_artifacts_for(peppy_root, self.name, NODE_TAG)
    }

    fn logs(&self, peppy_root: &Path) -> Vec<String> {
        build_logs_for(peppy_root, self.name, NODE_TAG)
    }
}

fn app_context(serve: &ServeCommandEmulation) -> Arc<AppContext> {
    Arc::new(
        AppContext::with_messenger(serve.temp_dir(), serve.messenger())
            .with_daemon_state_file(serve.daemon_state_path()),
    )
}

fn reused(log: &str) -> bool {
    log.contains(CACHED_BUILD_REUSE_PREFIX)
}

#[test]
fn node_build_reuses_the_artifact_of_identical_sources_and_rebuilds_on_change() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("serve emulation");
    let ctx = app_context(&serve);
    let node = CountedNode::scaffold(&ctx, "cache_probe", Toolchain::Cargo);
    let root = serve.temp_dir();

    node.add_and_build(&ctx, false);
    assert_eq!(node.builds(), 1);
    let first_artifacts = node.artifacts(root);
    assert_eq!(first_artifacts.len(), 1, "got {first_artifacts:?}");
    let logs = node.logs(root);
    assert_eq!(logs.len(), 1);
    assert!(!reused(&logs[0]), "a first build has nothing to reuse");

    node.add_and_build(&ctx, false);
    assert_eq!(node.builds(), 1, "identical sources must not run build_cmd");
    let logs = node.logs(root);
    assert_eq!(logs.len(), 2);
    assert!(
        reused(&logs[1]),
        "the second build reports the reuse:\n{}",
        logs[1]
    );
    assert_eq!(node.artifacts(root), first_artifacts);

    let main_rs = node.path.join("src").join("main.rs");
    let mut source = std::fs::read_to_string(&main_rs).expect("read main.rs");
    source.push_str("\n// a change to the node sources\n");
    std::fs::write(&main_rs, source).expect("write main.rs");

    node.add_and_build(&ctx, false);
    assert_eq!(node.builds(), 2, "changed sources build again");
    let logs = node.logs(root);
    assert_eq!(logs.len(), 3);
    assert!(!reused(&logs[2]), "a miss reports no reuse:\n{}", logs[2]);
    let artifacts = node.artifacts(root);
    assert_eq!(
        artifacts.len(),
        1,
        "one artifact per node identity, got {artifacts:?}"
    );
    assert_ne!(
        artifacts, first_artifacts,
        "a changed tree has a new fingerprint"
    );
}

/// The Python scaffold generates a test harness into the staged tree. Every
/// add stages into a fresh directory, so the harness must not bake that
/// directory's path, or identical sources would never share a fingerprint.
#[test]
fn node_build_reuses_the_artifact_of_identical_python_sources() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("serve emulation");
    let ctx = app_context(&serve);
    let node = CountedNode::scaffold(&ctx, "python_cache_probe", Toolchain::Uv);
    let root = serve.temp_dir();

    node.add_and_build(&ctx, false);
    node.add_and_build(&ctx, false);
    assert_eq!(
        node.builds(),
        1,
        "identical Python sources staged twice must share one fingerprint"
    );
    let logs = node.logs(root);
    assert_eq!(logs.len(), 2);
    assert!(
        reused(&logs[1]),
        "the second build reports the reuse:\n{}",
        logs[1]
    );
    assert_eq!(node.artifacts(root).len(), 1);
}

#[test]
fn node_build_rebuild_flag_bypasses_a_cache_hit() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("serve emulation");
    let ctx = app_context(&serve);
    let node = CountedNode::scaffold(&ctx, "rebuild_probe", Toolchain::Cargo);
    let root = serve.temp_dir();

    node.add_and_build(&ctx, false);
    node.add_and_build(&ctx, false);
    assert_eq!(node.builds(), 1);
    let artifacts = node.artifacts(root);

    node.add_and_build(&ctx, true);
    assert_eq!(node.builds(), 2, "--rebuild runs build_cmd despite the hit");
    let logs = node.logs(root);
    assert_eq!(logs.len(), 3);
    assert!(
        !reused(&logs[2]),
        "a rebuild reports no reuse:\n{}",
        logs[2]
    );
    assert!(
        logs[2].contains("Rebuilding rebuild_probe:v1"),
        "the bypass is announced:\n{}",
        logs[2]
    );
    assert_eq!(
        node.artifacts(root),
        artifacts,
        "an unchanged tree publishes over the same slot"
    );
}

#[test]
fn node_build_reuses_the_artifact_after_a_daemon_restart() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let root = TempDir::new().expect("peppy root tempdir");

    let serve = rt
        .block_on(ServeCommandEmulation::with_mock_in(root.path()))
        .expect("serve emulation");
    let ctx = app_context(&serve);
    let node = CountedNode::scaffold(&ctx, "restart_probe", Toolchain::Cargo);
    node.add_and_build(&ctx, false);
    assert_eq!(node.builds(), 1);
    drop(ctx);
    drop(serve);

    let serve = rt
        .block_on(ServeCommandEmulation::with_mock_in(root.path()))
        .expect("serve emulation over the same root");
    let ctx = app_context(&serve);
    node.add_and_build(&ctx, false);
    assert_eq!(
        node.builds(),
        1,
        "a restarted daemon finds the artifact on disk"
    );
    let logs = node.logs(root.path());
    assert_eq!(logs.len(), 2);
    assert!(reused(&logs[1]), "the reuse is reported:\n{}", logs[1]);
}
