//! Shape rules for the environment variables a spawned node receives.
//!
//! Two callers share them. The CLI filters the caller environment it forwards
//! with a launch or run goal, and the launcher parser rejects a per-instance
//! `env_vars` entry a node could not receive intact, and an `env_from_launch`
//! name that the forwarded environment can never hold. One definition keeps
//! the two in step: what the CLI silently drops from the ambient environment is
//! not something a launcher file can smuggle through, or wait for.
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

/// The characters [`is_safe_env_value`] accepts, as a message names them.
pub const SAFE_ENV_VALUE_CHARACTERS: &str = "letters, digits and `_ - . / : , = @ + %`";

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

/// The variables of the caller's environment that the CLI never forwards to a
/// node, because the node runs somewhere the caller's paths do not describe: a
/// different working directory (the instance dir) and, for a container node, a
/// different filesystem and often a different OS.
///
/// The temp-dir trio is the second kind. A node that asks for scratch space
/// resolves it from these, and the caller's value names a host path the
/// container cannot write: on macOS `TMPDIR` is always `/var/folders/…`,
/// which inside the guest exists at most as the read-only parent of a
/// bind-mounted peppy root, so the node dies with `EROFS` the first time it
/// wants a temp file. Dropping them lets resolution fall back to the
/// container's own writable `/tmp`. Apptainer's `--cleanenv` does not cover
/// this: these arrive as `APPTAINERENV_` variables the daemon sets on purpose,
/// which it has no reason to strip. All three are listed because the Rust and
/// Python lookups differ (`TMPDIR` alone, versus `TMPDIR`, `TEMP`, `TMP` in
/// order) and this stack runs both kinds of node.
///
/// The Python pair is a third kind: worse than naming host paths the node
/// cannot see, a forwarded value *displaces* the image's own. Apptainer
/// gives `--env` precedence over the image's `%environment`, so a caller
/// shell that exports `PYTHONPATH` (a ROS `setup.bash`, say) erases the
/// vendored `peppylib`/`peppygen` entries a Python container node boots
/// from: the node dies at import time with `No module named 'peppylib'`,
/// and only on machines whose shell exports one, which is why it escapes
/// local reproduction. `PYTHONHOME` fails harder still: it points the
/// container's interpreter at a host prefix, and Python cannot even find
/// its stdlib. A node that genuinely wants either var gets it deliberately,
/// via the launcher's `env_vars`.
///
/// The launch environment never holds these names, so a launcher cannot take
/// one of them from it either.
pub const NEVER_FORWARDED_ENV_KEYS: [&str; 7] = [
    "PWD",
    "OLDPWD",
    "TMPDIR",
    "TEMP",
    "TMP",
    "PYTHONPATH",
    "PYTHONHOME",
];

/// Whether `name` is one of the [`NEVER_FORWARDED_ENV_KEYS`].
/// Case-insensitive, as [`is_forbidden_env_name`] is.
pub fn is_never_forwarded_env_name(name: &str) -> bool {
    NEVER_FORWARDED_ENV_KEYS.contains(&name.trim().to_ascii_uppercase().as_str())
}

/// Why an explicitly declared environment variable cannot be given to a node.
/// Only declarations a human wrote (a launcher instance's `env_vars` and the
/// names its `env_from_launch` takes) produce this: the ambient caller
/// environment is filtered, not rejected, because it carries entries the user
/// never asked a node to see.
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
         hold {SAFE_ENV_VALUE_CHARACTERS} (no whitespace, quotes, or shell metacharacters)"
    )]
    Value { name: String },
    #[error(
        "env var `{name}` is never forwarded by the CLI: it names a working directory, a temp \
         dir or a Python installation of the caller's machine, so the launch environment never \
         holds it"
    )]
    NeverForwarded { name: String },
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

/// Checks the name of a variable that an instance takes from the launch
/// environment: a name [`check_env_var`] accepts, which the forwarded
/// environment of a launch can hold, so not one of the
/// [`NEVER_FORWARDED_ENV_KEYS`]. The value is checked where it arrives: the
/// CLI forwards only a value [`is_safe_env_value`] accepts.
pub fn check_launch_variable_name(name: &str) -> Result<(), InvalidEnvVar> {
    check_env_var(name, "")?;
    if is_never_forwarded_env_name(name) {
        return Err(InvalidEnvVar::NeverForwarded {
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

    /// A launch variable follows the name rules of `env_vars`, and is no
    /// name that the forwarded environment of a launch can never hold.
    #[test]
    fn a_launch_variable_name_is_one_the_launch_environment_can_hold() {
        assert_eq!(check_launch_variable_name("GEMINI_API_KEY"), Ok(()));
        assert_eq!(
            check_launch_variable_name("has-dash"),
            Err(InvalidEnvVar::Name {
                name: "has-dash".to_string()
            })
        );
        assert_eq!(
            check_launch_variable_name("LD_PRELOAD"),
            Err(InvalidEnvVar::Forbidden {
                name: "LD_PRELOAD".to_string()
            })
        );
        for name in NEVER_FORWARDED_ENV_KEYS
            .iter()
            .map(|name| name.to_string())
            .chain(["pythonpath".to_string()])
        {
            assert_eq!(
                check_launch_variable_name(&name),
                Err(InvalidEnvVar::NeverForwarded { name: name.clone() }),
                "`{name}` is never forwarded"
            );
        }
    }
}
