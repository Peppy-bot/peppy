//! Reporting build-script progress to the person running the build.
//!
//! Cargo captures a build script's stdout, keeps every `cargo:warning=` line
//! it finds, and replays them on every later build of that package, including
//! the fully cached ones where the script never runs again. Progress reported
//! that way outlives the work it described: a one-off tool compile keeps
//! reprinting its whole log on every command until something invalidates the
//! build script. Progress therefore goes straight to the terminal running the
//! build, which cargo neither captures nor replays, and `cargo:warning=` stays
//! reserved for conditions the user has to act on.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};

/// Cargo paints its status bar on the last terminal line and leaves the cursor
/// there with no trailing newline. `\r` returns to the start of that line and
/// `\x1b[2K` erases it, so a progress line replaces the bar instead of landing
/// on top of it; cargo repaints the bar on its next tick.
const ERASE_STATUS_BAR: &str = "\r\x1b[2K";

/// Reports one line of build-script progress on the terminal running the build.
///
/// Prefer the [`progress!`](crate::progress!) macro at call sites; this is the
/// function it forwards to. When the build has no controlling terminal (CI, an
/// IDE, a piped build) the message is dropped: nobody is watching for it, and
/// the alternative, a `cargo:warning=` line, would be replayed on every later
/// build. Failures are reported as warnings instead, so a build that goes
/// wrong stays diagnosable without a terminal.
pub fn report_progress(message: &str) {
    let Some(terminal) = terminal() else {
        return;
    };
    // A poisoned lock means another thread panicked mid-write. Progress is not
    // worth propagating that panic into the build, so the message is dropped.
    let Ok(mut terminal) = terminal.lock() else {
        return;
    };
    // One `write_all` per line keeps a line intact when concurrent build
    // scripts write to the same terminal.
    terminal
        .write_all(progress_line(package_name().as_deref(), message).as_bytes())
        .ok();
    terminal.flush().ok();
}

/// The line written to the terminal for `message`.
///
/// Cargo prefixes build-script warnings with the emitting package, so progress
/// carries the same prefix and stays attributable when several build scripts
/// report at once.
fn progress_line(package: Option<&str>, message: &str) -> String {
    match package {
        Some(package) => format!("{ERASE_STATUS_BAR}{package}: {message}\n"),
        None => format!("{ERASE_STATUS_BAR}{message}\n"),
    }
}

/// The package whose build script is reporting, from the environment cargo
/// sets for build scripts.
fn package_name() -> Option<String> {
    std::env::var("CARGO_PKG_NAME")
        .ok()
        .filter(|n| !n.is_empty())
}

/// The terminal running the build, opened once per build-script process.
///
/// `/dev/tty` is the process's controlling terminal, which a build script
/// inherits from the cargo that spawned it. Opening it bypasses the pipes
/// cargo reads stdout and stderr through, so what goes there reaches the user
/// live and is never recorded in the build-script output cargo replays.
fn terminal() -> Option<&'static Mutex<File>> {
    static TERMINAL: OnceLock<Option<Mutex<File>>> = OnceLock::new();
    TERMINAL
        .get_or_init(|| {
            OpenOptions::new()
                .write(true)
                .open("/dev/tty")
                .ok()
                .map(Mutex::new)
        })
        .as_ref()
}

/// Reports one line of build-script progress, formatted like `println!`.
///
/// ```ignore
/// build_helpers::progress!("Using cached {name} binary from {path:?}");
/// ```
#[macro_export]
macro_rules! progress {
    ($($arg:tt)*) => {
        $crate::report_progress(&::std::format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_line_carries_the_package_prefix() {
        assert_eq!(
            progress_line(Some("pmi"), "Compiling zenohd"),
            "\r\u{1b}[2Kpmi: Compiling zenohd\n"
        );
    }

    #[test]
    fn progress_line_without_a_package_holds_the_bare_message() {
        assert_eq!(
            progress_line(None, "Compiling zenohd"),
            "\r\u{1b}[2KCompiling zenohd\n"
        );
    }

    #[test]
    fn progress_line_erases_the_status_bar_before_every_message() {
        for message in ["", "one", "two words"] {
            assert!(
                progress_line(Some("pkg"), message).starts_with(ERASE_STATUS_BAR),
                "{message:?} should be preceded by the erase sequence"
            );
        }
    }

    #[test]
    fn progress_line_ends_every_message_with_a_newline() {
        assert!(progress_line(Some("pkg"), "no trailing newline").ends_with('\n'));
    }
}
