//! Shared ANSI color palette for the CLI's boxed views, so `stack list`,
//! `stack benchmark`, `repo list`, and `repo search` tint the same things the
//! same way (node labels cyan, link ids yellow, …). Applied only when
//! `colorize` is set; the table width logic strips these codes before
//! measuring, so a colored cell occupies the same display columns as its
//! plain text and stays aligned.

pub(super) const NODE_COLOR: &str = "\x1b[36m"; // cyan: node labels
pub(super) const COUNT_COLOR: &str = "\x1b[32m"; // green: per-node instance counts
pub(super) const INSTANCE_COLOR: &str = "\x1b[35m"; // magenta: instance ids
pub(super) const BINDING_COLOR: &str = "\x1b[33m"; // yellow: slot bindings / link ids
pub(super) const MEASURE_SERVICE_COLOR: &str = "\x1b[34m"; // blue: svc-probe measurement
pub(super) const MEASURE_ACTION_COLOR: &str = "\x1b[35m"; // magenta: act-probe measurement
pub(super) const MEASURE_NODE_COLOR: &str = "\x1b[36m"; // cyan: node-probe measurement (topic edges)
pub(super) const MEASURE_DELIVERY_COLOR: &str = "\x1b[32m"; // green: topic delivery measurement
pub(super) const STATUS_RUNNING_COLOR: &str = "\x1b[32m"; // green: a running instance
pub(super) const STATUS_STARTING_COLOR: &str = "\x1b[33m"; // yellow: a starting instance
pub(super) const STATUS_FINISHED_COLOR: &str = "\x1b[34m"; // blue: a cleanly-finished instance
pub(super) const STATUS_FAILED_COLOR: &str = "\x1b[31m"; // red: a crashed instance
pub(super) const HEALTH_HEALTHY_COLOR: &str = "\x1b[32m"; // green: a healthy instance
pub(super) const HEALTH_UNHEALTHY_COLOR: &str = "\x1b[31m"; // red: an unhealthy instance
pub(super) const RESET: &str = "\x1b[0m";

/// Orange, for the states that are not errors but are not the plain answer
/// either: entries kept from an earlier read, and an identity a
/// higher-priority repository already answers.
pub(super) const ORANGE: &str = "\x1b[38;5;208m";
/// Red, for what does not resolve at all.
pub(super) const RED: &str = "\x1b[31m";

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

#[cfg(test)]
mod tests {
    use super::paint;

    #[test]
    fn paint_is_a_no_op_without_colour() {
        assert_eq!(paint(false, "\x1b[31m", "  (conflict)"), "  (conflict)");
        assert_eq!(
            paint(true, "\x1b[31m", "  (conflict)"),
            "\x1b[31m  (conflict)\x1b[0m"
        );
    }
}
