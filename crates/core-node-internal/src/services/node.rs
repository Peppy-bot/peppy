mod add;
mod add_batch;
mod archive;
mod builder;
pub(crate) mod cache;
pub(crate) mod common;
mod env;
mod feedback;
pub(crate) mod gate;
mod git_utils;
mod info;
mod init;
mod logging;
pub(crate) mod pairing;
mod remove;
mod run;
mod stop;
mod sync;

use config::node::NodeConfig;
use core_node_api::encoding::NodeSource;
use daemon_config::consts::PeppyDirs;

// Crate-external re-exports (paths that other crates / other `services::`
// submodules reference as `crate::services::node::X`).
pub use add::listen_for_node_add;
pub use builder::listen_for_node_build;
pub use info::listen_for_node_info;
pub use init::listen_for_node_init;
pub use pairing::PairingCoordinator;
pub use remove::listen_for_node_remove;
pub use run::{DaemonDefaults, NodeRunServiceConfig, listen_for_node_run};
pub use stop::{
    TEARDOWN_REAP_BUDGET, force_kill_deadline, listen_for_node_stop, teardown_all_instances,
};
pub use sync::listen_for_node_sync;

pub(crate) use add::{NodeAddActionContext, log_label_from_source, run_node_add};
pub(crate) use builder::{NodeBuildActionContext, run_node_build_for_entity};
pub(crate) use feedback::{FeedbackLine, FeedbackStream};
pub(crate) use git_utils::{checkout_repo_ref, clone_with_progress, format_bytes};
pub(crate) use logging::{create_action_log_file, write_error_to_log};
pub(crate) use run::{NodeRunActionContext, resolve_mount_path_parameters, run_node_run};
pub(crate) use sync::resolve_contract_doc;

// Intra-`services::node::` re-imports: bring helper names back into the
// module root so sibling command files can reach them via `super::X`.
// Private on purpose: nothing outside this module needs these paths.
use archive::{
    extract_tar_zst, is_supported_fs_archive, is_supported_http_archive, locate_node_root_dir,
    resolve_local_archive_source,
};
use common::{encode_response_or_err, generate_random_id, panic_message};
use env::{inject_node_runtime_env, inject_rust_build_env, validate_goal_env_vars};
use feedback::spawn_feedback_forwarder;
use git_utils::{clone_repo_with_deadline, sanitize_repo_path};
use logging::append_stack_log;

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

pub(crate) async fn resolve_node_config(
    source: NodeSource,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<NodeConfig, String> {
    info::resolve_node_config(source, peppy_dirs).await
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
            env_vars
                .iter()
                .find(|(k, _)| k == "RUSTC_WRAPPER")
                .unwrap()
                .1,
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

        let apptainer_bin = env_vars
            .iter()
            .find(|(k, _)| k == PEPPY_APPTAINER_BIN_ENV_VAR)
            .map(|(_, v)| v)
            .expect("PEPPY_APPTAINER_BIN should be injected");
        assert!(
            !apptainer_bin.trim().is_empty(),
            "PEPPY_APPTAINER_BIN should not be empty"
        );

        assert_eq!(
            env_vars
                .iter()
                .find(|(k, _)| k == PEPPY_NODE_NAME_ENV_VAR)
                .map(|(_, v)| v.as_str()),
            Some("uvc_camera")
        );
        assert_eq!(
            env_vars
                .iter()
                .find(|(k, _)| k == PEPPY_NODE_TAG_ENV_VAR)
                .map(|(_, v)| v.as_str()),
            Some("v1")
        );
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
            env_vars
                .iter()
                .find(|(k, _)| k == PEPPY_APPTAINER_BIN_ENV_VAR)
                .map(|(_, v)| v.as_str()),
            Some("/custom/apptainer")
        );
        assert_eq!(
            env_vars
                .iter()
                .find(|(k, _)| k == PEPPY_NODE_NAME_ENV_VAR)
                .map(|(_, v)| v.as_str()),
            Some("custom_node")
        );
        assert_eq!(
            env_vars
                .iter()
                .find(|(k, _)| k == PEPPY_NODE_TAG_ENV_VAR)
                .map(|(_, v)| v.as_str()),
            Some("v999")
        );
    }

    #[test]
    fn sanitize_repo_path_accepts_relative_path() {
        let result = sanitize_repo_path("some/path");
        assert_eq!(result.unwrap(), PathBuf::from("some/path"));
    }

    #[test]
    fn sanitize_repo_path_strips_leading_slashes() {
        let result = sanitize_repo_path("///some/path");
        assert_eq!(result.unwrap(), PathBuf::from("some/path"));
    }

    #[test]
    fn sanitize_repo_path_strips_leading_backslashes() {
        let result = sanitize_repo_path("\\\\some\\path");
        assert_eq!(result.unwrap(), PathBuf::from("some\\path"));
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

    #[test]
    fn is_supported_http_archive_accepts_tar_zst() {
        let url = url::Url::parse("https://example.com/bundle.tar.zst").unwrap();
        assert!(is_supported_http_archive(&url));
    }

    #[test]
    fn is_supported_http_archive_accepts_tar_zstd() {
        let url = url::Url::parse("https://example.com/bundle.tar.zstd").unwrap();
        assert!(is_supported_http_archive(&url));
    }

    #[test]
    fn is_supported_http_archive_accepts_tzst() {
        let url = url::Url::parse("https://example.com/bundle.tzst").unwrap();
        assert!(is_supported_http_archive(&url));
    }

    #[test]
    fn is_supported_http_archive_rejects_tar_gz() {
        let url = url::Url::parse("https://example.com/bundle.tar.gz").unwrap();
        assert!(!is_supported_http_archive(&url));
    }

    #[test]
    fn is_supported_http_archive_rejects_plain_url() {
        let url = url::Url::parse("https://example.com/page").unwrap();
        assert!(!is_supported_http_archive(&url));
    }

    #[test]
    fn is_supported_http_archive_is_case_insensitive() {
        let url = url::Url::parse("https://example.com/BUNDLE.TAR.ZST").unwrap();
        assert!(is_supported_http_archive(&url));
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
