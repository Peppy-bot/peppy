use std::io::{self, BufRead, Write};

use crate::error::{Error, Result};

/// Prompts the user with a message and waits for y/yes confirmation.
/// If `reader` is provided, reads from it; otherwise reads from stdin.
pub(crate) fn confirm_prompt(message: &str, reader: Option<&mut dyn BufRead>) -> Result<bool> {
    print!("{message}");
    io::stdout()
        .flush()
        .map_err(|e| Error::ExecutionFailed(format!("Failed to write confirmation prompt: {e}")))?;

    let mut input = String::new();
    if let Some(reader) = reader {
        reader.read_line(&mut input)
    } else {
        io::stdin().read_line(&mut input)
    }
    .map_err(|e| Error::ExecutionFailed(format!("Failed to read confirmation response: {e}")))?;

    let response = input.trim().to_ascii_lowercase();
    Ok(matches!(response.as_str(), "y" | "yes"))
}

/// Formats instance IDs as a comma-separated quoted list.
pub(crate) fn format_instance_ids(instance_ids: &[String]) -> String {
    instance_ids
        .iter()
        .map(|id| format!("\"{id}\""))
        .collect::<Vec<_>>()
        .join(", ")
}
