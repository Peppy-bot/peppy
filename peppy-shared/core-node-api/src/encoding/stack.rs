pub mod benchmark;
pub mod budgets;
pub mod join;
pub mod launch;
pub mod list;
pub mod remove;
pub mod reset;

use crate::Result;
use crate::encoding::read_text_list;

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
