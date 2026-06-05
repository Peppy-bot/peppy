//! Shared ANSI color palette for the `stack` subcommands, so `stack list` and
//! `stack benchmark` tint the same things the same way (node labels cyan, link
//! ids yellow, …). Applied only when `colorize` is set; the table width logic in
//! each command strips these codes before measuring, so a colored cell occupies
//! the same display columns as its plain text and stays aligned.

pub(super) const NODE_COLOR: &str = "\x1b[36m"; // cyan — node labels
pub(super) const COUNT_COLOR: &str = "\x1b[32m"; // green — per-node instance counts
pub(super) const INSTANCE_COLOR: &str = "\x1b[35m"; // magenta — instance ids
pub(super) const BINDING_COLOR: &str = "\x1b[33m"; // yellow — slot bindings / link ids
pub(super) const STATUS_RUNNING_COLOR: &str = "\x1b[32m"; // green — a running instance
pub(super) const STATUS_STARTING_COLOR: &str = "\x1b[33m"; // yellow — a starting instance
pub(super) const HEALTH_HEALTHY_COLOR: &str = "\x1b[32m"; // green — a healthy instance
pub(super) const HEALTH_UNHEALTHY_COLOR: &str = "\x1b[31m"; // red — an unhealthy instance
pub(super) const RESET: &str = "\x1b[0m";

/// Wraps `s` in `code`/reset when `colorize` is set, otherwise returns it
/// unchanged. Empty input is left untouched so blank continuation cells don't
/// carry dangling escape codes.
pub(super) fn paint(colorize: bool, code: &str, s: &str) -> String {
    if colorize && !s.is_empty() {
        format!("{code}{s}{RESET}")
    } else {
        s.to_string()
    }
}
