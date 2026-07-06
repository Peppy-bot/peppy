use core_node_api::FORBIDDEN_ENV_KEYS;

/// Env vars that are meaningless or misleading when forwarded to a spawned node
/// because the node runs in a different working directory (the instance dir).
const CALLER_ONLY_ENV_KEYS: [&str; 2] = ["PWD", "OLDPWD"];

/// Whether `name` is a valid POSIX shell identifier (`[A-Za-z_][A-Za-z0-9_]*`).
///
/// Only such names can be forwarded to a containerized node. A node runs under
/// apptainer, which writes every forwarded `--env NAME=value` into a generated
/// `/.inject-apptainer-env.sh` that the container sources at startup. A name
/// that is not a shell identifier makes that `source` abort with "invalid var
/// name", and because the abort happens mid-script every later var is dropped,
/// including `PEPPY_RUNTIME_CONFIG`. The node then silently falls back to its
/// standalone defaults instead of the daemon-provided parameters.
///
/// The caller's environment routinely contains such names: bash exports shell
/// functions under keys like `BASH_FUNC_foo%%`, and other tooling uses keys
/// with `%`, `(`, or `.`. None of these are meaningful to a node, so dropping
/// them is both safe and necessary.
fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first == '_' || first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Whether `value` is safe to forward to a containerized node.
///
/// Same root cause as [`is_valid_env_name`]: apptainer writes each forwarded
/// `--env NAME=value` UNQUOTED into the `/.inject-apptainer-env.sh` the
/// container sources, effectively `export NAME=value`. A value with whitespace
/// makes the shell read trailing words as further export targets (`export X=a b`
/// tries to export `b`), which aborts the script with "invalid var name" and
/// drops every later var, including `PEPPY_RUNTIME_CONFIG`. Worse, a value with
/// shell metacharacters is a command-injection vector (`X=a;cmd` would run
/// `cmd` inside the container). Neither shape is meaningful for a node's
/// environment, so we forward only values built from characters that survive an
/// unquoted assignment unchanged.
fn is_safe_env_value(value: &str) -> bool {
    value.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | '-' | '.' | '/' | ':' | ',' | '=' | '@' | '+' | '%')
    })
}

/// Whether a caller env var should be forwarded to a spawned node: it must have
/// a valid shell-identifier name (see [`is_valid_env_name`]) and a value safe to
/// inject unquoted (see [`is_safe_env_value`]), not be a code injection vector,
/// and not be a caller-only var that is wrong in the node's working directory.
fn should_forward_env(key: &str, value: &str) -> bool {
    let normalized = key.trim().to_ascii_uppercase();
    is_valid_env_name(key)
        && is_safe_env_value(value)
        && !FORBIDDEN_ENV_KEYS.contains(&normalized.as_str())
        && !CALLER_ONLY_ENV_KEYS.contains(&normalized.as_str())
}

/// Collects environment variables from the caller's environment to pass to the daemon.
/// Filters out forbidden env vars that could be used for code injection,
/// caller-only vars that would be incorrect in the node's working directory,
/// vars whose names are not valid shell identifiers (see [`is_valid_env_name`]),
/// and vars whose values are not safe to inject unquoted (see [`is_safe_env_value`]).
pub fn caller_env_overrides() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(key, value)| should_forward_env(key, value))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_identifier_env_names() {
        // Bash exports functions under names like this; they break the
        // container's env-injection script if forwarded.
        assert!(!is_valid_env_name("BASH_FUNC_foo%%"));
        assert!(!is_valid_env_name("foo bar"));
        assert!(!is_valid_env_name("foo.bar"));
        assert!(!is_valid_env_name("foo("));
        assert!(!is_valid_env_name("1foo"));
        assert!(!is_valid_env_name(""));
    }

    #[test]
    fn accepts_valid_identifier_env_names() {
        assert!(is_valid_env_name("PATH"));
        assert!(is_valid_env_name("PEPPY_RUNTIME_CONFIG"));
        assert!(is_valid_env_name("_underscore"));
        assert!(is_valid_env_name("ROS_DOMAIN_ID"));
        assert!(is_valid_env_name("X1"));
    }

    #[test]
    fn rejects_unsafe_env_values() {
        // Whitespace splits an unquoted `export X=a b` and aborts the script.
        assert!(!is_safe_env_value("user:inference user:file_upload"));
        assert!(!is_safe_env_value("a\tb"));
        assert!(!is_safe_env_value("line1\nline2"));
        // Shell metacharacters are command-injection vectors.
        assert!(!is_safe_env_value("a;rm -rf /"));
        assert!(!is_safe_env_value("$(whoami)"));
        assert!(!is_safe_env_value("`id`"));
        assert!(!is_safe_env_value("a|b"));
    }

    #[test]
    fn accepts_safe_env_values() {
        // Real-world safe values: paths, lists, ids, urls, ratios.
        assert!(is_safe_env_value("/opt/node/.venv/bin:/usr/bin"));
        assert!(is_safe_env_value("42"));
        assert!(is_safe_env_value("sentry-release=app%401.2,public_key=abc"));
        assert!(is_safe_env_value("https://example.com/path"));
        assert!(is_safe_env_value(""));
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
