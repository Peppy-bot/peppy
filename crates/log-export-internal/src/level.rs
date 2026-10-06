//! Reads the level a captured line prints.

use daemon_config::peppy_config::Severity;
use regex::Regex;
use std::sync::LazyLock;

/// The level a line prints: its severity and the token as printed.
pub(crate) struct PrintedLevel<'a> {
    pub severity: Severity,
    pub text: &'a str,
}

/// The first word of a line after an optional timestamp, with the character
/// that ends it. The timestamp is a date and a time, joined by `T` or a
/// space, as Rust `tracing` and Python `logging` print them.
static FIRST_WORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        ^\s*
        (?: \[? \d{4}[-/]\d{2}[-/]\d{2} [T\ ] \d{2}:\d{2}:\d{2} (?:[.,]\d+)? (?:Z|[+-]\d{2}:?\d{2})? \]? )?
        [\s\-\[|]*
        (?P<word> [A-Za-z]+ )
        (?P<end> [:\[\]\s] | $ )
        ",
    )
    .expect("the level pattern is valid")
});

/// The level `line` prints, when its first word after an optional timestamp
/// names one. `line` holds no terminal escape sequences.
///
/// A word that ends at `:` or `[` is a level in any letter case (`error:`,
/// `Error:`, `error[E0308]`, `WARNING:root:`). A word that ends at a space, at
/// `]` or at the end of the line is a level in uppercase only (`INFO text`,
/// `[WARN]`), so a sentence that starts with a level word is no level.
pub(crate) fn printed_level(line: &str) -> Option<PrintedLevel<'_>> {
    let captures = FIRST_WORD.captures(line)?;
    let text = captures.name("word")?.as_str();
    let end = captures.name("end").map_or("", |end| end.as_str());
    let reads_any_case = matches!(end, ":" | "[");
    let is_uppercase = text.bytes().all(|byte| byte.is_ascii_uppercase());
    if !reads_any_case && !is_uppercase {
        return None;
    }
    let severity = severity_of(text)?;
    Some(PrintedLevel { severity, text })
}

/// The severity a level name stands for, in any letter case. `warning` is
/// warn and `critical` is fatal, as Python's `logging` prints them.
fn severity_of(name: &str) -> Option<Severity> {
    const ALIASES: [(&str, Severity); 2] =
        [("warning", Severity::Warn), ("critical", Severity::Fatal)];
    Severity::ALL
        .into_iter()
        .map(|severity| (severity.name(), severity))
        .chain(ALIASES)
        .find(|(known, _)| known.eq_ignore_ascii_case(name))
        .map(|(_, severity)| severity)
}

/// `line` without its terminal escape sequences.
pub(crate) fn without_escapes(line: &str) -> String {
    anstream::adapter::strip_str(line).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(line: &str) -> Option<(Severity, &str)> {
        printed_level(line).map(|level| (level.severity, level.text))
    }

    #[test]
    fn the_level_after_a_timestamp_is_read() {
        for (line, expected) in [
            (
                "2026-09-21T23:33:53.793516Z  INFO zenoh::net::runtime: Using ZID: 1d2e",
                (Severity::Info, "INFO"),
            ),
            (
                "  2026-09-21T23:33:54.318976Z  WARN node::control: scouting delay elapsed",
                (Severity::Warn, "WARN"),
            ),
            (
                "2026-09-21 16:33:46,774 - INFO - publishing clock domain",
                (Severity::Info, "INFO"),
            ),
            (
                "2026-09-21 16:33:46,774 - CRITICAL - the scene is closed",
                (Severity::Fatal, "CRITICAL"),
            ),
            (
                "[2026-09-21T16:33:46+02:00] [ERROR] the arm stopped",
                (Severity::Error, "ERROR"),
            ),
        ] {
            assert_eq!(level(line), Some(expected), "{line}");
        }
    }

    #[test]
    fn the_level_at_the_start_of_a_line_is_read() {
        for (line, expected) in [
            ("INFO the node is ready", (Severity::Info, "INFO")),
            ("DEBUG tick 12", (Severity::Debug, "DEBUG")),
            ("TRACE", (Severity::Trace, "TRACE")),
            ("[WARN] battery low", (Severity::Warn, "WARN")),
            ("WARNING:root:battery low", (Severity::Warn, "WARNING")),
            ("INFO:    Extracting image", (Severity::Info, "INFO")),
            ("FATAL:   could not open image", (Severity::Fatal, "FATAL")),
        ] {
            assert_eq!(level(line), Some(expected), "{line}");
        }
    }

    #[test]
    fn a_word_before_a_colon_or_a_bracket_is_read_in_any_case() {
        for (line, expected) in [
            (
                "error: cannot update the lock file",
                (Severity::Error, "error"),
            ),
            ("Error: Node(HardFault)", (Severity::Error, "Error")),
            ("error[E0308]: mismatched types", (Severity::Error, "error")),
            ("warning: unused variable `x`", (Severity::Warn, "warning")),
            ("info: downloading component", (Severity::Info, "info")),
        ] {
            assert_eq!(level(line), Some(expected), "{line}");
        }
    }

    #[test]
    fn a_line_that_prints_no_level_has_none() {
        for line in [
            "",
            "the node is ready",
            "Info about the node",
            "Error handling started",
            "ERRORS found: 3",
            "[recorder] started",
            "+ apt-get update",
            "Traceback (most recent call last):",
            "  File \"main.py\", line 3, in <module>",
            "FileNotFoundError: no such file",
            "2026-09-21 16:33:46,774 - recorder - INFO - started",
            "2026/09/18 09:18:52  warn rootless{usr/bin/sudo} ignoring EPERM",
            "control loop: 60 Hz",
        ] {
            assert_eq!(level(line), None, "{line}");
        }
    }

    #[test]
    fn escape_sequences_are_removed() {
        let line = "\u{1b}[2m2026-09-21T23:33:53.793516Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m \
                    \u{1b}[2mzenoh::net::runtime\u{1b}[0m\u{1b}[2m:\u{1b}[0m Using ZID";
        let plain = without_escapes(line);
        assert_eq!(
            plain,
            "2026-09-21T23:33:53.793516Z  INFO zenoh::net::runtime: Using ZID"
        );
        assert_eq!(level(&plain), Some((Severity::Info, "INFO")));
    }

    #[test]
    fn a_level_name_is_read_in_any_case() {
        for (name, expected) in [
            ("TRACE", Severity::Trace),
            ("debug", Severity::Debug),
            ("Info", Severity::Info),
            ("WARN", Severity::Warn),
            ("WARNING", Severity::Warn),
            ("error", Severity::Error),
            ("CRITICAL", Severity::Fatal),
            ("fatal", Severity::Fatal),
        ] {
            assert_eq!(severity_of(name), Some(expected), "{name}");
        }
        for name in ["", "note", "information", "errors", "warn1"] {
            assert_eq!(severity_of(name), None, "{name}");
        }
    }
}
