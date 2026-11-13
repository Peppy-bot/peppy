use daemon_config::env::{check_env_var, is_never_forwarded_env_name};

/// Whether a caller env var should be forwarded to a spawned node: it must
/// carry a name and a value a node can receive intact (see [`daemon_config::env`],
/// which states why and is the same rule the launcher's `env_vars` are parsed
/// against), must not be a code injection vector, and must not be a caller-only
/// var that is wrong in the node's working directory
/// ([`daemon_config::env::NEVER_FORWARDED_ENV_KEYS`]).
///
/// This side filters rather than rejects. The caller environment is ambient and
/// full of entries nobody asked a node to see, so a failing entry is dropped
/// silently; an `env_vars` entry a user wrote in a launcher file meets the same
/// rules but fails loudly, while the file is being parsed.
fn should_forward_env(key: &str, value: &str) -> bool {
    check_env_var(key, value).is_ok() && !is_never_forwarded_env_name(key)
}

/// Collects environment variables from the caller's environment to pass to the daemon.
/// Filters out forbidden env vars that could be used for code injection,
/// caller-only vars that would be incorrect in the node's working directory,
/// and vars whose name or value a node could not receive intact (see
/// [`should_forward_env`]).
///
/// What is collected here reaches the daemon this process talks to and no
/// further: these values describe this machine, so a launch that places an
/// instance on a peer daemon dispatches its goals without them.
pub(crate) fn caller_env_overrides() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(key, value)| should_forward_env(key, value))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The caller's temp dir describes the host, not the container. On macOS
    /// it is `/var/folders/…`, which the guest cannot write, so forwarding it
    /// kills any node that asks for scratch space (this is how
    /// `openarm_backbone` died: `tempfile::tempdir()` for its collision
    /// meshes, `EROFS`). Both filters accept the value (a valid identifier,
    /// and a value of only alphanumerics, `/` and `_`), so it has to be
    /// excluded by name.
    #[test]
    fn drops_caller_temp_dir_vars() {
        for key in ["TMPDIR", "TEMP", "TMP"] {
            assert!(
                !should_forward_env(key, "/var/folders/kb/36lp35_92z5/T/"),
                "{key} describes the caller's filesystem and must not reach the container"
            );
        }
    }

    /// The caller's Python vars describe the caller's interpreter, not the
    /// node's. Forwarding them is not merely useless: an `APPTAINERENV_`
    /// variable outranks the image's `%environment`, so a forwarded `PYTHONPATH`
    /// replaces the vendored peppylib/peppygen path a Python container node
    /// imports from (this is how `sim_mujoco` died with
    /// `No module named 'peppylib'` on a machine whose shell exported a ROS
    /// `PYTHONPATH`), and a forwarded `PYTHONHOME` points the interpreter at
    /// a prefix the guest does not have. Both filters accept the values
    /// (valid identifiers, path-only values), so they have to be excluded
    /// by name.
    #[test]
    fn drops_caller_python_env_vars() {
        for key in ["PYTHONPATH", "PYTHONHOME", "pythonpath"] {
            assert!(
                !should_forward_env(key, "/opt/ros/jazzy/lib/python3.12/site-packages"),
                "{key} describes the caller's Python installation and must not reach the node"
            );
        }
    }

    /// The exclusion is by whole name, not prefix: a node's own configuration
    /// must survive.
    #[test]
    fn keeps_env_vars_that_merely_mention_excluded_names() {
        assert!(should_forward_env("TMPDIR_OVERRIDE", "/opt/scratch"));
        assert!(should_forward_env("PEPPY_TMP", "/opt/scratch"));
        assert!(should_forward_env("PYTHONPATH_EXTRA", "/opt/plugins"));
    }

    #[test]
    fn should_forward_env_drops_junk_keeps_useful() {
        // Bash-exported function: dropped because the name is not an identifier.
        assert!(!should_forward_env("BASH_FUNC_demo%%", "() { :; }"));
        // Space-valued var (e.g. OAuth scopes): dropped because the shell
        // that sources it reads the space as the end of the value.
        assert!(!should_forward_env("OAUTH_SCOPES", "read write admin"));
        // A `~` is expanded by the shell that sources the value, so a key
        // that holds one is dropped, and a launch that needs it says so.
        assert!(!should_forward_env("GEMINI_API_KEY", "key~with-tilde"));
        // Code-injection vector: dropped by the forbidden list.
        assert!(!should_forward_env("LD_PRELOAD", "/evil.so"));
        // Caller-only: dropped because it is wrong in the node's working dir.
        assert!(!should_forward_env("PWD", "/home/user"));
        // Ordinary vars a node may legitimately want are forwarded.
        assert!(should_forward_env("ROS_DOMAIN_ID", "42"));
        assert!(should_forward_env(
            "PEPPY_RUNTIME_CONFIG",
            "/path/to/config.json5"
        ));
    }
}
