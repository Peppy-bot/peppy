//! The output of `peppy node call`, `echo` and `describe` on standard
//! output, which a program reads: one JSON value per line for `call` and
//! `echo`.

use serde_json::Value;
use std::io::{ErrorKind, Write};

/// Writes `value` and a line feed, then flushes, so a program that reads
/// the output gets each line as soon as it is written.
pub(crate) fn write_json_line(out: &mut impl Write, value: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    out.write_all(b"\n")?;
    out.flush()
}

/// Whether `error` says that the program that reads the output closed it,
/// for example `head -n 5` once it has its lines: the end that the reader
/// asked for, not an error of the command.
pub(crate) fn is_closed_by_reader(error: &std::io::Error) -> bool {
    error.kind() == ErrorKind::BrokenPipe
}
