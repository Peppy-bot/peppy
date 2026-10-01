use std::io::{self, BufRead, Write};

use crate::error::{Error, Result};

/// Prompts the user with a message and waits for y/yes confirmation.
/// If `reader` is provided, reads from it; otherwise reads from stdin.
pub(crate) fn confirm_prompt(message: &str, reader: Option<&mut dyn BufRead>) -> Result<bool> {
    let response = read_answer(message, reader)?.unwrap_or_default();
    Ok(matches!(
        response.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Prints `prompt` and reads one line of answer from `reader`, else from
/// stdin. `None` at the end of the input: there is then no person to ask.
fn read_answer(prompt: &str, reader: Option<&mut (dyn BufRead + '_)>) -> Result<Option<String>> {
    print!("{prompt}");
    io::stdout()
        .flush()
        .map_err(|e| Error::ExecutionFailed(format!("Failed to write the prompt: {e}")))?;

    let mut input = String::new();
    let read = match reader {
        Some(reader) => reader.read_line(&mut input),
        None => io::stdin().read_line(&mut input),
    }
    .map_err(|e| Error::ExecutionFailed(format!("Failed to read the answer: {e}")))?;
    Ok((read > 0).then_some(input))
}

/// Prints `options` as a numbered list under `title` and asks the person to
/// select one `what` by its number. Returns the index of the selected option.
/// If `reader` is provided, reads from it; otherwise reads from stdin.
///
/// An answer that is not a number of the list prints the range and asks
/// again. End of input is an error: there is then no person to ask.
pub(crate) fn choose_prompt(
    title: &str,
    what: &str,
    options: &[String],
    mut reader: Option<&mut dyn BufRead>,
) -> Result<usize> {
    println!("{title}");
    for (index, option) in options.iter().enumerate() {
        println!("  {}) {option}", index + 1);
    }
    loop {
        let prompt = format!("Select a {what} [1-{}]: ", options.len());
        let Some(input) = read_answer(&prompt, reader.as_deref_mut())? else {
            return Err(Error::ExecutionFailed(format!("no {what} was selected")));
        };
        match parse_choice(&input, options.len()) {
            Some(index) => return Ok(index),
            None => println!("Type a number from 1 to {}.", options.len()),
        }
    }
}

/// The index an answer selects in a list of `count` options numbered from 1,
/// or `None` when the answer is not a number of the list.
fn parse_choice(answer: &str, count: usize) -> Option<usize> {
    let number: usize = answer.trim().parse().ok()?;
    (1..=count).contains(&number).then(|| number - 1)
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

    fn choose(input: &str) -> Result<usize> {
        let mut reader = Cursor::new(input.as_bytes().to_vec());
        choose_prompt(
            "Projects",
            "project",
            &["Lab".to_string(), "Field".to_string()],
            Some(&mut reader),
        )
    }

    #[test]
    fn a_number_of_the_list_selects_its_option() {
        assert_eq!(choose("1\n").unwrap(), 0);
        assert_eq!(choose("  2  \n").unwrap(), 1);
    }

    #[test]
    fn a_bad_answer_asks_again() {
        assert_eq!(choose("0\n3\nLab\n\n2\n").unwrap(), 1);
    }

    #[test]
    fn end_of_input_selects_nothing() {
        let err = choose("").expect_err("no person to ask");
        assert!(err.to_string().contains("no project was selected"), "{err}");
        assert!(choose("7\n").is_err(), "a bad answer, then end of input");
    }

    #[test]
    fn only_a_number_of_the_list_is_a_choice() {
        assert_eq!(parse_choice("1", 2), Some(0));
        assert_eq!(parse_choice("2\n", 2), Some(1));
        for bad in ["0", "3", "-1", "1.0", "one", ""] {
            assert_eq!(parse_choice(bad, 2), None, "{bad:?}");
        }
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
