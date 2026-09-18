//! Environment-variable protocol policy shared by both sides of the
//! core-node wire. Pure data, no I/O.

/// Blocklist of dangerous env vars that could be used for code injection or process manipulation.
/// Protocol policy enforced on both sides of the wire: the daemon rejects requests carrying
/// these keys (`core-node`'s `validate_goal_env_vars`), and the CLI filters the caller
/// environment before sending (`peppy`'s `should_forward_env`).
pub const FORBIDDEN_ENV_KEYS: [&str; 16] = [
    // Linux dynamic linker injection
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "LD_DEBUG",
    // macOS dynamic linker injection
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    // Shell injection vectors
    "BASH_ENV",
    "ENV",
    "CDPATH",
    "IFS",
    "SHELLOPTS",
    "BASHOPTS",
    "PS4",
    "PROMPT_COMMAND",
    "GLOBIGNORE",
];
