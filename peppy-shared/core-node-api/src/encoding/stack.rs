pub mod benchmark;
pub mod budgets;
pub mod join;
pub mod launch;
pub mod list;
pub mod remove;
pub mod reset;

use crate::Result;
use crate::encoding::read_text_list;

/// The reason a daemon gives when it refuses a stack change or a node goal
/// while another one holds its stack: the rejection reason of the goal, or
/// the error of the service. The daemon runs one stack change at a time, and
/// a node goal beside other node goals only. The daemon bridge of the
/// built-in MCP server refuses a second `join` of its own with this text too.
pub const STACK_BUSY_REASON: &str =
    "a stack or node operation is in progress on this daemon; wait for it to finish";

/// The shape of a `--with` entry, quoted in the refusal so the message says
/// what to type.
const SELECTION_GUIDANCE: &str =
    "a `--with` entry is `option` or `axis=option`, never blank (check for a stray comma)";

/// The `--with` words a goal carries, verbatim. A blank word is refused with
/// the shape of a real one: it is the empty segment of a caller's comma list.
pub(crate) fn read_selections(
    list: capnp::text_list::Reader<'_>,
    field: &str,
) -> Result<Vec<String>> {
    let words = read_text_list(list)?;
    match words.iter().position(String::is_empty) {
        Some(index) => Err(crate::Error::Decoding(format!(
            "`{field}[{index}]` is empty: {SELECTION_GUIDANCE}"
        ))),
        None => Ok(words),
    }
}
