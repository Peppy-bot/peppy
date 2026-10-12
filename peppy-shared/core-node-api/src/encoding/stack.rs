pub mod benchmark;
pub mod budgets;
pub mod join;
pub mod launch;
pub mod list;
pub mod remove;
pub mod reset;
pub mod words;

/// The reason a daemon gives when it refuses a stack change or a node goal
/// while another one holds its stack: the rejection reason of the goal, or
/// the error of the service. The daemon runs one stack change at a time, and
/// a node goal beside other node goals only. The daemon bridge of the
/// built-in MCP server refuses a second `join` of its own with this text too.
pub const STACK_BUSY_REASON: &str =
    "a stack or node operation is in progress on this daemon; wait for it to finish";
