use daemon_node::FORBIDDEN_ENV_KEYS;

/// Env vars that are meaningless or misleading when forwarded to a spawned node
/// because the node runs in a different working directory (the instance dir).
const CALLER_ONLY_ENV_KEYS: [&str; 2] = ["PWD", "OLDPWD"];

/// Collects environment variables from the caller's environment to pass to the daemon.
/// Filters out forbidden env vars that could be used for code injection, and
/// caller-only vars that would be incorrect in the node's working directory.
pub fn caller_env_overrides() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(key, _)| {
            let normalized = key.trim().to_ascii_uppercase();
            !FORBIDDEN_ENV_KEYS.contains(&normalized.as_str())
                && !CALLER_ONLY_ENV_KEYS.contains(&normalized.as_str())
        })
        .collect()
}
