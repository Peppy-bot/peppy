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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn answer(input: &str) -> bool {
        let mut reader = Cursor::new(input.as_bytes().to_vec());
        confirm_prompt("Proceed? ", Some(&mut reader)).expect("reading from a cursor never fails")
    }

    #[test]
    fn affirmative_answers_confirm() {
        assert!(answer("y\n"));
        assert!(answer("yes\n"));
        assert!(answer("Y\n"), "matching is case-insensitive");
        assert!(answer("YES\n"));
        assert!(answer("  yes  \n"), "surrounding whitespace is trimmed");
    }

    #[test]
    fn anything_else_declines() {
        assert!(!answer("n\n"));
        assert!(!answer("no\n"));
        assert!(!answer("\n"), "a bare newline declines");
        assert!(!answer(""), "EOF declines");
        assert!(!answer("yep\n"), "only y/yes count as yes");
    }

    #[test]
    fn format_instance_ids_quotes_and_joins() {
        assert_eq!(format_instance_ids(&[]), "");
        assert_eq!(format_instance_ids(&["a".to_string()]), "\"a\"");
        assert_eq!(
            format_instance_ids(&["a".to_string(), "b".to_string()]),
            "\"a\", \"b\""
        );
    }
}
