//! Registering the built-in MCP server in the node stack from a
//! `NodeSource::Exposures` add goal.
//!
//! The goal carries the pinned exposures and, in its closure, the pinned
//! contracts. This daemon materializes those bytes, derives the server's
//! identity, manifest and catalogs from them, writes the manifest and the
//! serve spec under its built-in nodes directory, and registers the node
//! ready to start with the `peppy` executable as its artifact. Nothing is
//! fetched for a node, generated or built.

use super::materialize_exposure_deployment;
use crate::services::node::{FeedbackLine, FeedbackStream, NodeAddActionContext, pins};
use core_node_api::encoding::{NodeAddGoal, NodeAddResult, NodeSource};
use daemon_config::consts::PeppyDirs;
use daemon_config::mcp_deployment::{RUN_COMMAND, SPEC_ENV_VAR};
use daemon_config::repository::{DeploymentPins, DeploymentRoot, PinKind};
use node_stack::BuiltInLaunch;
use parking_lot::Mutex as StdMutex;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;

/// The manifest file of a built-in node, beside its serve spec.
const MANIFEST_FILE: &str = config::consts::NODE_CONFIG_FILE;
/// The serve spec file of a built-in node.
const SPEC_FILE: &str = "mcp_serve.json5";

/// Where the `peppy` executable that serves built-in nodes is: this daemon's
/// own executable when it is `peppy`, otherwise the installed one in the
/// Peppy home's bin directory.
pub fn resolve_peppy_executable(peppy_dirs: &PeppyDirs) -> Result<PathBuf, String> {
    let expected = format!("peppy{}", std::env::consts::EXE_SUFFIX);
    let own = std::env::current_exe().ok();
    if let Some(path) = own.as_ref().filter(|path| {
        path.file_name()
            .is_some_and(|name| name == expected.as_str())
    }) {
        return Ok(path.clone());
    }
    let installed = peppy_dirs.bin_dir().join(&expected);
    if installed.is_file() {
        return Ok(installed);
    }
    Err(format!(
        "cannot find the `peppy` executable that serves built-in MCP nodes: this daemon runs as {} \
         and {} does not exist",
        own.map(|path| path.display().to_string())
            .unwrap_or_else(|| "an unknown executable".to_owned()),
        installed.display()
    ))
}

/// The spawn recipe of a built-in node whose documents live in `dir`.
fn built_in_launch(
    executable: PathBuf,
    peppy_dirs: &PeppyDirs,
    spec_path: &Path,
    http_paths: Vec<String>,
) -> BuiltInLaunch {
    BuiltInLaunch {
        executable,
        args: RUN_COMMAND[1..]
            .iter()
            .map(|arg| (*arg).to_owned())
            .collect(),
        env: vec![
            (SPEC_ENV_VAR.to_owned(), spec_path.display().to_string()),
            // The server compiles wire schemas through the bundled schema
            // compiler, which lives under the Peppy home; it must be this
            // daemon's home, whatever the caller's environment says.
            (
                config::consts::PEPPY_HOME_ENV.to_owned(),
                peppy_dirs.root().display().to_string(),
            ),
        ],
        http_paths,
    }
}

/// Entry point `dispatch_node_add` routes a `NodeSource::Exposures` goal to.
pub(crate) async fn run_built_in_add(
    goal: NodeAddGoal,
    action_context: NodeAddActionContext,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    log_file: Arc<StdMutex<File>>,
    log_path: PathBuf,
) -> NodeAddResult {
    let fail = |msg: String| {
        crate::services::node::write_error_to_log(&log_file, &msg);
        NodeAddResult::failure(&log_path, msg)
    };
    let NodeSource::Exposures { pins_json5 } = &goal.source else {
        return fail("internal error: run_built_in_add called with another source".to_owned());
    };
    let exposure_pins = match pins::decode_pins(pins_json5) {
        Ok(pins) => pins,
        Err(e) => return fail(format!("the goal's exposure pins: {e}")),
    };
    let closure = match pins::decode_pins(&goal.pins_json5) {
        Ok(closure) => closure,
        Err(e) => return fail(e),
    };
    if let Some(stray) = closure.iter().find(|pin| pin.kind != PinKind::Contract) {
        return fail(format!(
            "the closure of a built-in MCP deployment carries only contracts, got {}",
            stray.label()
        ));
    }
    let pins = match DeploymentPins::new(DeploymentRoot::Exposures(exposure_pins), closure) {
        Ok(pins) => pins,
        Err(e) => return fail(e),
    };

    let _ = feedback_tx.send(FeedbackLine {
        stream: FeedbackStream::Stdout,
        line: format!(
            "Registering the built-in MCP server for {}",
            pins.root.label()
        ),
    });

    let peppy_dirs = action_context.peppy_dirs.clone();
    let on_feedback: crate::services::node::cache::MaterializeFeedback = Arc::new(
        crate::services::node::stdout_line_sender(feedback_tx.clone()),
    );
    let resolved = {
        let dirs = peppy_dirs.clone();
        match tokio::task::spawn_blocking(move || {
            materialize_exposure_deployment(&dirs, &pins, &|line| on_feedback(line))
        })
        .await
        {
            Ok(Ok(resolved)) => resolved,
            Ok(Err(e)) => return fail(e),
            Err(e) => return fail(format!("materialization task failed: {e}")),
        }
    };

    let executable = match resolve_peppy_executable(&peppy_dirs) {
        Ok(path) => path,
        Err(e) => return fail(e),
    };

    let plan = resolved.plan;
    let dir = peppy_dirs.built_in_nodes_dir().join(plan.name.as_str());
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return fail(format!("cannot create {}: {e}", dir.display()));
    }
    let manifest_path = dir.join(MANIFEST_FILE);
    let manifest = match json5_pretty::to_string_pretty(&plan.config) {
        Ok(text) => text,
        Err(e) => return fail(format!("cannot serialize the synthesized manifest: {e}")),
    };
    if let Err(e) = daemon_config::atomic_write::publish_atomic(&manifest_path, |tmp| {
        std::fs::write(tmp, &manifest)
    }) {
        return fail(format!("cannot write {}: {e}", manifest_path.display()));
    }
    let spec_path = dir.join(SPEC_FILE);
    if let Err(e) = resolved.spec.write(&spec_path) {
        return fail(e);
    }

    let http_paths: Vec<String> = plan
        .exposures
        .iter()
        .map(|exposure| exposure.bundle.exposure.endpoint_path())
        .collect();
    let launch = built_in_launch(executable, &peppy_dirs, &spec_path, http_paths.clone());
    let name = plan.name.as_str().to_owned();
    let tag = plan.tag.clone();
    if let Err(e) = action_context
        .node_stack
        .push_built_in(plan.config, &manifest_path, launch)
    {
        return fail(format!(
            "cannot register `{name}:{tag}` in the node stack: {e}"
        ));
    }
    let _ = feedback_tx.send(FeedbackLine {
        stream: FeedbackStream::Stdout,
        line: format!(
            "Registered `{name}:{tag}`, serving {}",
            http_paths.join(", ")
        ),
    });
    NodeAddResult::success(log_path, name, tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_installed_executable_is_found_in_the_bin_dir() {
        let home = tempfile::tempdir().expect("temp home");
        let dirs = PeppyDirs::new(home.path());
        let error = resolve_peppy_executable(&dirs).expect_err("nothing installed yet");
        assert!(error.contains("bin"), "{error}");
        std::fs::create_dir_all(dirs.bin_dir()).expect("create bin dir");
        let installed = dirs
            .bin_dir()
            .join(format!("peppy{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&installed, "").expect("write a stand-in");
        assert_eq!(resolve_peppy_executable(&dirs).expect("found"), installed);
    }

    #[test]
    fn the_recipe_runs_the_serve_subcommand_with_the_spec_and_this_home() {
        let dirs = PeppyDirs::new("/home/robot/.peppy");
        let launch = built_in_launch(
            PathBuf::from("/home/robot/.peppy/bin/peppy"),
            &dirs,
            Path::new("/home/robot/.peppy/built_in/mcp_camera_v1/mcp_serve.json5"),
            vec!["/camera/v1/mcp".to_owned()],
        );
        assert_eq!(launch.args, ["mcp", "serve"]);
        assert_eq!(
            launch.env,
            [
                (
                    SPEC_ENV_VAR.to_owned(),
                    "/home/robot/.peppy/built_in/mcp_camera_v1/mcp_serve.json5".to_owned()
                ),
                ("PEPPY_HOME".to_owned(), "/home/robot/.peppy".to_owned()),
            ]
        );
        assert_eq!(
            launch.endpoint_urls(8900),
            ["http://127.0.0.1:8900/camera/v1/mcp"]
        );
    }
}
