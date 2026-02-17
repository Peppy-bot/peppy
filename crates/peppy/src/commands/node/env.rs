use daemon_node::FORBIDDEN_ENV_KEYS;

/// Collects environment variables from the caller's environment to pass to the daemon.
/// Filters out forbidden env vars that could be used for code injection.
pub fn caller_env_overrides() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(key, _)| {
            let normalized = key.trim().to_ascii_uppercase();
            !FORBIDDEN_ENV_KEYS.contains(&normalized.as_str())
        })
        .collect()
}
