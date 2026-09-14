//! Shape rules for the environment variables a spawned node receives.
//!
//! Two callers share them. The CLI filters the caller environment it forwards
//! with a launch or run goal, and the launcher parser rejects a per-instance
//! `env_vars` entry a node could not receive intact. One definition keeps the
//! two in step: what the CLI silently drops from the ambient environment is not
//! something a launcher file can smuggle through.
//!
//! The rules follow the strictest consumer, a container node. A variable
//! reaches one through the `APPTAINERENV_` prefix, and apptainer writes it into
//! a script the container sources at startup, where the shell expands the
//! value: `$HOME` becomes a path and `$(cmd)` runs `cmd` inside the container.
//! A name is a shell identifier so the assignment parses; a value is built from
//! characters the shell leaves alone so it arrives as written and executes
//! nothing. A process node is more forgiving, but a node gains or loses a
//! `container` block over its life while the launcher file that deploys it
//! stays the same, so one rule covers both.

use core_node_api::FORBIDDEN_ENV_KEYS;
use thiserror::Error;

/// Whether `name` is a valid POSIX shell identifier (`[A-Za-z_][A-Za-z0-9_]*`).
///
/// The caller environment routinely holds names that are not: bash exports
/// shell functions under keys like `BASH_FUNC_foo%%`, and other tooling uses
/// keys with `%`, `(`, or `.`. None are meaningful to a node.
pub fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first == '_' || first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Whether `value` reaches a node as written: letters, digits, and the
/// punctuation that appears in the values nodes actually take (device paths,
/// hosts, ports, search paths, log filters, tokens). The shell inside the
/// container expands what it is given, so a value carrying `$`, a backtick or
/// a quote would run there rather than arrive.
pub fn is_safe_env_value(value: &str) -> bool {
    value.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | '-' | '.' | '/' | ':' | ',' | '=' | '@' | '+' | '%')
    })
}

/// Whether `name` is one of the [`FORBIDDEN_ENV_KEYS`]: dynamic-linker and
/// shell hooks that turn an environment entry into code execution inside the
/// node. Case-insensitive, so a lowercase spelling cannot slip past.
pub fn is_forbidden_env_name(name: &str) -> bool {
    FORBIDDEN_ENV_KEYS.contains(&name.trim().to_ascii_uppercase().as_str())
}

/// Why an explicitly declared environment variable cannot be given to a node.
/// Only declarations a human wrote (a launcher instance's `env_vars`) produce
/// this: the ambient caller environment is filtered, not rejected, because it
/// carries entries the user never asked a node to see.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InvalidEnvVar {
    #[error(
        "env var name `{name}` is not a valid shell identifier: a name must start with a letter \
         or `_` and contain only letters, digits, and `_`"
    )]
    Name { name: String },
    #[error(
        "env var `{name}` is reserved: it is a dynamic-linker or shell hook that would let an \
         environment entry run code inside the node"
    )]
    Forbidden { name: String },
    #[error(
        "value of env var `{name}` contains characters a node cannot receive intact: a value may \
         hold letters, digits, and `_ - . / : , = @ + %` (no whitespace, quotes, or shell \
         metacharacters)"
    )]
    Value { name: String },
}

/// Checks one explicitly declared environment variable against the rules above.
/// Callers run this at their parse boundary so an entry that could not reach a
/// node is rejected where the user wrote it, not at spawn time with the node
/// half-deployed.
pub fn check_env_var(name: &str, value: &str) -> Result<(), InvalidEnvVar> {
    if !is_valid_env_name(name) {
        return Err(InvalidEnvVar::Name {
            name: name.to_string(),
        });
    }
    if is_forbidden_env_name(name) {
        return Err(InvalidEnvVar::Forbidden {
            name: name.to_string(),
        });
    }
    if !is_safe_env_value(value) {
        return Err(InvalidEnvVar::Value {
            name: name.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_names_must_be_shell_identifiers() {
        for name in [
            "PATH",
            "_underscore",
            "ESP32_DEVICE",
            "PEPPY_RUNTIME_CONFIG",
            "X1",
        ] {
            assert!(is_valid_env_name(name), "`{name}` should be valid");
        }
        // `BASH_FUNC_foo%%` is how bash exports a shell function; the rest are
        // the punctuation other tooling puts in keys.
        for name in [
            "",
            "1foo",
            "BASH_FUNC_foo%%",
            "foo bar",
            "foo.bar",
            "foo(",
            " PADDED",
        ] {
            assert!(!is_valid_env_name(name), "`{name}` should be rejected");
        }
    }

    #[test]
    fn env_values_must_reach_a_node_as_written() {
        // Real-world values: paths, endpoints, log filters, tokens, urls.
        for value in [
            "",
            "/dev/ttyUSB0",
            "/opt/node/.venv/bin:/usr/bin",
            "127.0.0.1:7448",
            "info,my_node=debug",
            "sentry-release=app%401.2,public_key=abc",
            "https://example.com/path",
            "42",
        ] {
            assert!(is_safe_env_value(value), "`{value}` should be accepted");
        }
        // The shell that sources them expands `$` and backticks, and reads
        // whitespace as the end of the value.
        for value in [
            "user:inference user:file_upload",
            "a\tb",
            "line1\nline2",
            "a;rm -rf /",
            "$(whoami)",
            "`id`",
            "a|b",
        ] {
            assert!(!is_safe_env_value(value), "`{value}` should be rejected");
        }
    }

    #[test]
    fn forbidden_names_are_matched_case_insensitively() {
        assert!(is_forbidden_env_name("LD_PRELOAD"));
        assert!(is_forbidden_env_name("ld_preload"));
        assert!(!is_forbidden_env_name("LD_PRELOADED"));
    }

    /// Each rejection names the offending variable and states its own rule, so
    /// the message a user sees points at what to change.
    #[test]
    fn check_reports_the_rule_that_failed() {
        assert_eq!(
            check_env_var("has-dash", "ok"),
            Err(InvalidEnvVar::Name {
                name: "has-dash".to_string()
            })
        );
        assert_eq!(
            check_env_var("LD_PRELOAD", "/tmp/evil.so"),
            Err(InvalidEnvVar::Forbidden {
                name: "LD_PRELOAD".to_string()
            })
        );
        assert_eq!(
            check_env_var("GREETING", "hello world"),
            Err(InvalidEnvVar::Value {
                name: "GREETING".to_string()
            })
        );
        assert_eq!(check_env_var("ESP32_DEVICE", "/dev/ttyUSB0"), Ok(()));
    }
}
