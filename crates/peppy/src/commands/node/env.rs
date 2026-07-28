use daemon_config::env::{is_forbidden_env_name, is_safe_env_value, is_valid_env_name};

/// Env vars that are meaningless or misleading when forwarded to a spawned node
/// because the node runs somewhere the caller's paths do not describe: a
/// different working directory (the instance dir) and, for a container node, a
/// different filesystem and often a different OS.
///
/// The temp-dir trio is the second kind. A node that asks for scratch space
/// resolves it from these, and the caller's value names a host path the
/// container cannot write — on macOS `TMPDIR` is always `/var/folders/…`,
/// which inside the guest exists at most as the read-only parent of a
/// bind-mounted peppy root, so the node dies with `EROFS` the first time it
/// wants a temp file. Dropping them lets resolution fall back to the
/// container's own writable `/tmp`. Apptainer's `--cleanenv` does not cover
/// this: these arrive as explicit `--env` flags, which it has no reason to
/// strip. All three are listed because the Rust and Python lookups differ
/// (`TMPDIR` alone, versus `TMPDIR`, `TEMP`, `TMP` in order) and this stack
/// runs both kinds of node.
const CALLER_ONLY_ENV_KEYS: [&str; 5] = ["PWD", "OLDPWD", "TMPDIR", "TEMP", "TMP"];

/// Whether a caller env var should be forwarded to a spawned node: it must
/// carry a name and a value a node can receive intact (see [`daemon_config::env`],
/// which states why and is the same rule the launcher's `env_vars` are parsed
/// against), must not be a code injection vector, and must not be a caller-only
/// var that is wrong in the node's working directory.
///
/// This side filters rather than rejects. The caller environment is ambient and
/// full of entries nobody asked a node to see, so a failing entry is dropped
/// silently; an `env_vars` entry a user wrote in a launcher file meets the same
/// rules but fails loudly, while the file is being parsed.
fn should_forward_env(key: &str, value: &str) -> bool {
    let normalized = key.trim().to_ascii_uppercase();
    is_valid_env_name(key)
        && is_safe_env_value(value)
        && !is_forbidden_env_name(key)
        && !CALLER_ONLY_ENV_KEYS.contains(&normalized.as_str())
}

/// Collects environment variables from the caller's environment to pass to the daemon.
/// Filters out forbidden env vars that could be used for code injection,
/// caller-only vars that would be incorrect in the node's working directory,
/// and vars whose name or value a node could not receive intact (see
/// [`should_forward_env`]).
pub fn caller_env_overrides() -> Vec<(String, String)> {
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
    /// meshes, `EROFS`). Both filters accept the value — valid identifier,
    /// value is only alphanumerics, `/` and `_` — so it has to be excluded by
    /// name.
    #[test]
    fn drops_caller_temp_dir_vars() {
        for key in ["TMPDIR", "TEMP", "TMP"] {
            assert!(
                !should_forward_env(key, "/var/folders/kb/36lp35_92z5/T/"),
                "{key} describes the caller's filesystem and must not reach the container"
            );
        }
    }

    /// The exclusion is by whole name, not prefix: a node's own configuration
    /// must survive.
    #[test]
    fn keeps_env_vars_that_merely_mention_temp() {
        assert!(should_forward_env("TMPDIR_OVERRIDE", "/opt/scratch"));
        assert!(should_forward_env("PEPPY_TMP", "/opt/scratch"));
    }

    #[test]
    fn should_forward_env_drops_junk_keeps_useful() {
        // Bash-exported function: dropped because the name is not an identifier.
        assert!(!should_forward_env("BASH_FUNC_demo%%", "() { :; }"));
        // Space-valued var (e.g. OAuth scopes): dropped because it breaks the
        // unquoted env injection and silently loses later vars.
        assert!(!should_forward_env("OAUTH_SCOPES", "read write admin"));
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
