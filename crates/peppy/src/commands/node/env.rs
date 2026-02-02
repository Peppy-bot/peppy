pub fn caller_env_overrides() -> Vec<(String, String)> {
    const ENV_KEYS: [&str; 4] = ["PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME"];

    ENV_KEYS
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| (key.to_string(), value))
        })
        .collect()
}
