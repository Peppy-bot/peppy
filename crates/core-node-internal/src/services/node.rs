mod add;
mod add_batch;
mod archive;
pub(crate) mod binding;
mod builder;
pub(crate) mod cache;
pub(crate) mod common;
pub mod endpoints;
mod env;
mod feedback;
pub(crate) mod gate;
mod git_utils;
mod health_monitor;
mod info;
mod init;
pub(crate) mod observation;
pub(crate) mod pairing;
pub(crate) mod pins;
mod relationship_notify;
mod remove;
mod run;
mod sets;
mod stack_log;
mod stop;
mod supervisor;
mod sync;
#[cfg(test)]
pub(crate) mod test_git_daemon;

use config::node::NodeConfig;
use core_node_api::encoding::InstanceEnd;
use node_stack::{KeptRelations, NodeStack, Registrations};
use std::sync::Arc;

// Crate-external re-exports (paths that other crates / other `services::`
// submodules reference as `crate::services::node::X`).
pub use add::listen_for_node_add;
pub use binding::BindingCoordinator;
pub use builder::listen_for_node_build;
pub use health_monitor::HealthMonitorPolicy;
pub use info::listen_for_node_info;
pub use init::listen_for_node_init;
pub use node_stack::RestartBackoff;
pub use observation::ObservationCoordinator;
pub use pairing::PairingCoordinator;
pub(crate) use relationship_notify::RelationshipNotifier;
pub use remove::listen_for_node_remove;
pub use run::{DaemonDefaults, NodeRunServiceConfig, listen_for_node_run};
pub use stack_log::ExportDiscardLog;
pub use stop::{
    TEARDOWN_REAP_BUDGET, force_kill_deadline, listen_for_node_stop, node_remove_bound,
    node_stop_bound, teardown_all_instances,
};
pub(crate) use stop::{
    named_instances, stop_named_instance, stop_named_instances, teardown_timeout,
};
pub use sync::listen_for_node_sync;

pub(crate) use add::{NodeAddActionContext, create_add_log, dispatch_node_add, record_add_failure};
pub(crate) use builder::{NodeBuildActionContext, run_node_build_for_entity};
pub use feedback::Report;
pub(crate) use feedback::{
    FeedbackLine, FeedbackStream, interface_step_sink, step_sink, trace_interface_step,
    unlogged_step_sink,
};
pub(crate) use git_utils::{
    ShallowCheckoutError, checkout_repo_ref, clone_with_progress, shallow_checkout_of_read_ref,
};
pub(crate) use run::{
    NodeRunActionContext, assemble_runtime_config, resolve_mount_path_parameters, run_node_run,
    start_give_up_budget,
};
pub(crate) use sync::resolve_contract_doc;

// Intra-`services::node::` re-imports: bring helper names back into the
// module root so sibling command files can reach them via `super::X`.
// Private on purpose: nothing outside this module needs these paths.
use archive::{
    extract_tar_zst, is_supported_fs_archive, is_supported_http_archive, locate_node_root_dir,
    resolve_local_archive_source,
};
use common::{
    encode_hex, encode_response_or_err, generate_random_id, panic_message, parse_node_config_file,
};
use env::{inject_node_runtime_env, inject_rust_build_env, validate_goal_env_vars};
use feedback::spawn_feedback_forwarder;
use git_utils::sanitize_repo_path;

// Test-only re-imports (referenced exclusively from the `tests` module below).
#[cfg(test)]
use config::consts::NODE_CONFIG_FILE;
#[cfg(test)]
use config::node::PeppygenLanguage;
#[cfg(test)]
use env::{
    PEPPY_APPTAINER_BIN_ENV_VAR, PEPPY_NODE_NAME_ENV_VAR, PEPPY_NODE_TAG_ENV_VAR,
    is_sccache_available,
};
#[cfg(test)]
use std::path::{Path, PathBuf};

/// SHA256 of a resolved node manifest, over its canonical pretty-printed
/// serialization so two daemons that resolved the same manifest agree
/// byte-for-byte regardless of how their sources were formatted.
///
/// One function because the fingerprint has two consumers that MUST agree: the
/// `node_info` service reports it to a human, and a federated launch pins it in
/// the instance plan so a participant can refuse a manifest that changed under
/// it between preflight and dispatch. Computing it two ways would make that
/// refusal fire on formatting.
pub(crate) fn manifest_fingerprint(config: &NodeConfig) -> std::result::Result<String, String> {
    let serialized = json5_pretty::to_string_pretty(config)
        .map_err(|e| format!("failed to serialize node config for fingerprinting: {e}"))?;
    Ok(config::fingerprint::fingerprint_for_bytes(
        serialized.as_bytes(),
    ))
}

/// The daemon authorities responsible for relationships between node instances.
///
/// Keeping them together gives every lifecycle path one complete dependency for
/// teardown while still exposing each authority to flows that operate on one
/// relationship type directly. The set deliveries a join or a removal makes
/// span two of them, and go through [`RelationshipCoordinators::replace_sets`]
/// in `sets.rs`.
#[derive(Clone)]
pub(crate) struct RelationshipCoordinators {
    /// The instances this daemon runs, which the sets it replaces must belong to.
    node_stack: Arc<NodeStack>,
    pairing: Arc<PairingCoordinator>,
    observation: Arc<ObservationCoordinator>,
    binding: Arc<BindingCoordinator>,
    notifier: Arc<RelationshipNotifier>,
}

impl RelationshipCoordinators {
    pub(crate) fn new(
        node_stack: Arc<NodeStack>,
        pairing: Arc<PairingCoordinator>,
        observation: Arc<ObservationCoordinator>,
        binding: Arc<BindingCoordinator>,
        notifier: Arc<RelationshipNotifier>,
    ) -> Self {
        Self {
            node_stack,
            pairing,
            observation,
            binding,
            notifier,
        }
    }

    pub(crate) fn pairing(&self) -> &Arc<PairingCoordinator> {
        &self.pairing
    }

    pub(crate) fn observation(&self) -> &Arc<ObservationCoordinator> {
        &self.observation
    }

    pub(crate) fn notifier(&self) -> &Arc<RelationshipNotifier> {
        &self.notifier
    }

    /// Ends every cross-instance relationship of an instance a stop path
    /// ends on purpose, keyed on its id whether or not the stack still
    /// tracks it: takes its registrations, forgets the pairs its local peers
    /// held with it, and announces its end as stopped
    /// ([`Self::announce_down`]), on which a remote peer forgets the pair
    /// too. `kept` is what the removed slot kept: after a self-exit emptied
    /// the registries, its held pairs name the remote peers and its kept
    /// watchers the daemons to tell. A self-exit keeps the held pairs for
    /// the restart it may wait for, so the exit watcher calls
    /// `take_registrations` and `announce_down` itself.
    pub(crate) async fn tear_down_instance(&self, instance_id: &str, kept: KeptRelations) {
        let registrations = self.take_registrations(instance_id);
        self.forget_pairs_held_with(&self.node_stack.core_node_name(), instance_id);
        let told = registrations
            .watchers
            .into_iter()
            .chain(kept.watchers)
            .chain(kept.pair_daemons);
        self.announce_down(instance_id, InstanceEnd::Stopped, told)
            .await;
    }

    /// Takes what `instance_id` registered as an observer and the watchers
    /// of its lifecycle, synchronously, so an exit can take them under the
    /// slot's own lock before anything observes the new state.
    pub(crate) fn take_registrations(&self, instance_id: &str) -> Registrations {
        Registrations {
            observations: self.observation.take_observations(instance_id),
            watchers: self.notifier.take_watchers(instance_id),
        }
    }

    /// The announcements of an instance's end, after
    /// [`Self::take_registrations`]: the end to the daemons in `told` and to
    /// its pair peers, then its pairs dissolved with each survivor told, then
    /// its observers told the source went down. The end is announced first
    /// because its recipients include the peers of the pairs the dissolution
    /// removes; pairs are dissolved before the source announcement, so a peer
    /// hears of its slot first.
    pub(crate) async fn announce_down(
        &self,
        instance_id: &str,
        end: InstanceEnd,
        told: impl IntoIterator<Item = String>,
    ) {
        self.notifier.announce_end(instance_id, end, told).await;
        self.pairing.dissolve_for_instance(instance_id).await;
        self.observation.announce_source_down(instance_id).await;
    }

    /// Forgets, on every local instance, the pairs held with the instance
    /// `instance_id` of the daemon `core_node`, which a stop ended on purpose.
    pub(crate) fn forget_pairs_held_with(&self, core_node: &str, instance_id: &str) {
        self.node_stack
            .forget_pairs_held_with(core_node, instance_id);
    }

    /// Records the daemons whose observers tap `instance_id`, as a launch,
    /// join or removal names them: in the registry a running instance
    /// announces to, and on the slot of a `restarting` instance for its next
    /// process.
    pub(crate) fn record_watchers(&self, instance_id: &str, core_nodes: &[String]) {
        self.notifier.set_watchers(instance_id, core_nodes);
        self.node_stack
            .record_watchers_for_restart(instance_id, core_nodes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_NODE_CONFIG: &str = r#"{
  peppy_schema: "node/v1",
  manifest: {
    name: "standalone",
    tag: "v1",
  },
  interfaces: {},
  execution: {
    language: "rust",
    parameters: {
      device_path: "string",
      video: {
        frame_rate: "u16",
        resolution: {
          width: "u16",
          height: "u16",
        },
        encoding: "string",
      },
    },
    build_cmd: [
      "cargo",
      "build",
      "--release",
    ],
    run_cmd: [
      "./target/release/standalone",
    ],
  },
}"#;

    fn write_tar_zst_archive(archive_path: &Path, entries: &[(&str, Option<&str>)]) {
        let file = std::fs::File::create(archive_path).unwrap();
        let encoder = zstd::stream::write::Encoder::new(file, 1).unwrap();
        let mut builder = tar::Builder::new(encoder);

        for (path, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_path(path).unwrap();
            header.set_mode(if contents.is_some() { 0o644 } else { 0o755 });
            if let Some(contents) = contents {
                header.set_size(contents.len() as u64);
                header.set_cksum();
                builder.append(&header, contents.as_bytes()).unwrap();
            } else {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                header.set_cksum();
                builder.append(&header, std::io::empty()).unwrap();
            }
        }

        builder.finish().unwrap();
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();
    }

    /// The value `env_vars` holds for `key`, if any.
    fn env_value<'a>(env_vars: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env_vars
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    /// A manifest without `execution.setup_timeout_secs` keeps the
    /// fingerprint it has in every peppy release, so a federated launch
    /// between daemons of two releases agrees on it. The value is pinned.
    #[test]
    fn a_manifest_without_a_setup_budget_keeps_its_fingerprint() {
        let config = config::node::NodeConfigParser::from_content(TEST_NODE_CONFIG)
            .expect("the test manifest parses");
        assert_eq!(
            manifest_fingerprint(&config).expect("the manifest serializes"),
            "e370133366998fc3a940b41c5443d797728a4e1456be7fc5246acd92e00e027f"
        );
    }

    #[test]
    fn inject_rust_build_env_skips_python_nodes() {
        let mut env_vars = vec![("FOO".to_string(), "bar".to_string())];
        inject_rust_build_env(&mut env_vars, PeppygenLanguage::Python);
        assert!(
            !env_vars.iter().any(|(k, _)| k == "RUSTC_WRAPPER"),
            "RUSTC_WRAPPER should not be set for Python nodes"
        );
    }

    #[test]
    fn inject_rust_build_env_respects_user_overrides() {
        let mut env_vars = vec![("RUSTC_WRAPPER".to_string(), "custom_wrapper".to_string())];
        inject_rust_build_env(&mut env_vars, PeppygenLanguage::Rust);
        assert_eq!(
            env_vars
                .iter()
                .filter(|(k, _)| k == "RUSTC_WRAPPER")
                .count(),
            1,
            "should not duplicate RUSTC_WRAPPER"
        );
        assert_eq!(
            env_value(&env_vars, "RUSTC_WRAPPER").unwrap(),
            "custom_wrapper",
            "should keep user-provided RUSTC_WRAPPER value"
        );
    }

    #[test]
    fn is_sccache_available_does_not_panic() {
        let _ = is_sccache_available();
    }

    #[test]
    fn inject_node_runtime_env_sets_expected_keys() {
        let mut env_vars = Vec::new();
        inject_node_runtime_env(&mut env_vars, "uvc_camera", "v1");

        let apptainer_bin = env_value(&env_vars, PEPPY_APPTAINER_BIN_ENV_VAR)
            .expect("PEPPY_APPTAINER_BIN should be injected");
        assert!(
            !apptainer_bin.trim().is_empty(),
            "PEPPY_APPTAINER_BIN should not be empty"
        );

        assert_eq!(
            env_value(&env_vars, PEPPY_NODE_NAME_ENV_VAR),
            Some("uvc_camera")
        );
        assert_eq!(env_value(&env_vars, PEPPY_NODE_TAG_ENV_VAR), Some("v1"));
    }

    #[test]
    fn inject_node_runtime_env_keeps_existing_values() {
        let mut env_vars = vec![
            (
                PEPPY_APPTAINER_BIN_ENV_VAR.to_string(),
                "/custom/apptainer".to_string(),
            ),
            (
                PEPPY_NODE_NAME_ENV_VAR.to_string(),
                "custom_node".to_string(),
            ),
            (PEPPY_NODE_TAG_ENV_VAR.to_string(), "v999".to_string()),
        ];

        inject_node_runtime_env(&mut env_vars, "uvc_camera", "v1");

        assert_eq!(
            env_vars
                .iter()
                .filter(|(k, _)| k == PEPPY_APPTAINER_BIN_ENV_VAR)
                .count(),
            1
        );
        assert_eq!(
            env_value(&env_vars, PEPPY_APPTAINER_BIN_ENV_VAR),
            Some("/custom/apptainer")
        );
        assert_eq!(
            env_value(&env_vars, PEPPY_NODE_NAME_ENV_VAR),
            Some("custom_node")
        );
        assert_eq!(env_value(&env_vars, PEPPY_NODE_TAG_ENV_VAR), Some("v999"));
    }

    #[rstest::rstest]
    #[case::relative("some/path", "some/path")]
    #[case::leading_slashes("///some/path", "some/path")]
    #[case::leading_backslashes("\\\\some\\path", "some\\path")]
    fn sanitize_repo_path_accepts_a_path_inside_the_repository(
        #[case] input: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(sanitize_repo_path(input).unwrap(), PathBuf::from(expected));
    }

    #[test]
    fn sanitize_repo_path_rejects_parent_dir() {
        let result = sanitize_repo_path("some/../etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains(".."));
    }

    #[test]
    fn sanitize_repo_path_rejects_empty_input() {
        assert!(sanitize_repo_path("").is_err());
        assert!(sanitize_repo_path("   ").is_err());
        assert!(sanitize_repo_path("///").is_err());
    }

    #[rstest::rstest]
    #[case::tar_zst("https://example.com/bundle.tar.zst", true)]
    #[case::tar_zstd("https://example.com/bundle.tar.zstd", true)]
    #[case::tzst("https://example.com/bundle.tzst", true)]
    #[case::tar_gz("https://example.com/bundle.tar.gz", false)]
    #[case::plain_url("https://example.com/page", false)]
    #[case::upper_case("https://example.com/BUNDLE.TAR.ZST", true)]
    fn is_supported_http_archive_accepts_only_zstd_tarballs(
        #[case] url: &str,
        #[case] expected: bool,
    ) {
        let url = url::Url::parse(url).unwrap();
        assert_eq!(is_supported_http_archive(&url), expected);
    }

    #[test]
    fn is_supported_fs_archive_accepts_supported_extensions() {
        assert!(is_supported_fs_archive(Path::new("bundle.tar.zst")));
        assert!(is_supported_fs_archive(Path::new("bundle.tar.zstd")));
        assert!(is_supported_fs_archive(Path::new("bundle.tzst")));
        assert!(is_supported_fs_archive(Path::new("BUNDLE.TAR.ZST")));
        assert!(!is_supported_fs_archive(Path::new("bundle.tar.gz")));
    }

    /// Verifies that `resolve_local_archive_source` works when the config file
    /// sits directly at the archive root (no wrapping top-level folder).
    #[test]
    fn resolve_local_archive_source_accepts_root_layout() {
        let temp_dir = tempfile::tempdir().unwrap();
        let archive_path = temp_dir.path().join("bundle.tar.zst");
        write_tar_zst_archive(&archive_path, &[(NODE_CONFIG_FILE, Some(TEST_NODE_CONFIG))]);

        let resolved = resolve_local_archive_source(&archive_path).unwrap();

        assert_eq!(resolved.node_config.manifest.name.as_str(), "standalone");
        assert_eq!(resolved.source_path, resolved.temp_dir.path());
        assert!(resolved.source_path.join(NODE_CONFIG_FILE).is_file());
    }

    /// Verifies that `resolve_local_archive_source` unwraps a single top-level
    /// folder and uses it as the source path instead of the extraction root.
    #[test]
    fn resolve_local_archive_source_uses_single_top_level_folder() {
        let temp_dir = tempfile::tempdir().unwrap();
        let archive_path = temp_dir.path().join("bundle.tar.zst");
        write_tar_zst_archive(
            &archive_path,
            &[("node", None), ("node/peppy.json5", Some(TEST_NODE_CONFIG))],
        );

        let resolved = resolve_local_archive_source(&archive_path).unwrap();

        assert_eq!(resolved.node_config.manifest.name.as_str(), "standalone");
        assert_eq!(
            resolved
                .source_path
                .file_name()
                .and_then(|name| name.to_str()),
            Some("node")
        );
        assert_eq!(
            resolved.source_path.parent(),
            Some(resolved.temp_dir.path())
        );
        assert!(resolved.source_path.join(NODE_CONFIG_FILE).is_file());
    }

    #[test]
    fn validate_goal_env_vars_preserves_key_casing() {
        let env_vars = vec![
            ("my_Custom_Var".to_string(), "value1".to_string()),
            ("AnotherVar".to_string(), "value2".to_string()),
        ];
        let result = validate_goal_env_vars(&env_vars).unwrap();
        assert_eq!(result[0].0, "my_Custom_Var");
        assert_eq!(result[1].0, "AnotherVar");
    }

    #[test]
    fn validate_goal_env_vars_trims_key_whitespace() {
        let env_vars = vec![("  MY_VAR  ".to_string(), "value".to_string())];
        let result = validate_goal_env_vars(&env_vars).unwrap();
        assert_eq!(result[0].0, "MY_VAR");
    }

    #[test]
    fn validate_goal_env_vars_rejects_forbidden_keys_case_insensitively() {
        let env_vars = vec![("ld_preload".to_string(), "evil.so".to_string())];
        let err = validate_goal_env_vars(&env_vars).unwrap_err();
        assert!(err.to_string().contains("LD_PRELOAD"));
    }

    #[test]
    fn panic_message_extracts_str_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("something broke");
        assert_eq!(panic_message(&*payload), "something broke");
    }

    #[test]
    fn panic_message_extracts_string_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(String::from("detailed error"));
        assert_eq!(panic_message(&*payload), "detailed error");
    }

    #[test]
    fn panic_message_handles_unknown_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(42i32);
        assert_eq!(panic_message(&*payload), "unknown panic payload");
    }
}
