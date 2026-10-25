//! The output of `peppy node call` and `peppy node echo`: one JSON value per
//! line on standard output, which a program reads line by line.

use serde_json::Value;
use std::io::Write;

/// Writes `value` and a line feed, then flushes, so a program that reads
/// the output gets each line as soon as it is written.
pub(crate) fn write_json_line(out: &mut impl Write, value: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    out.write_all(b"\n")?;
    out.flush()
}
