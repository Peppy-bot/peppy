//! The descriptions of documents as the commands print them: each paragraph
//! wrapped at its spaces to the width of the terminal, under an indent.

use super::table::{col_width, wrap};
use config::description::DocumentDescription;

/// `text` as lines that start with `indent`, wrapped by [`wrap`] so no line
/// is wider than `max_width` columns; a word wider than a line breaks at the
/// end of the line. `None` keeps the text on one line. The whitespace of
/// `text` collapses to one space, and each line ends with a newline.
pub(crate) fn wrapped(text: &str, indent: &str, max_width: Option<usize>) -> String {
    let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let lines = match max_width {
        Some(width) => wrap(&words, width.saturating_sub(col_width(indent))),
        None => words,
    };
    lines
        .lines()
        .map(|line| format!("{indent}{line}\n"))
        .collect()
}

/// The paragraphs of a document description, each [`wrapped`], with a
/// blank line between two paragraphs.
pub(crate) fn paragraphs(
    description: &DocumentDescription,
    indent: &str,
    max_width: Option<usize>,
) -> String {
    description
        .paragraphs()
        .iter()
        .map(|paragraph| wrapped(paragraph.as_str(), indent, max_width))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::description::Description;

    #[test]
    fn a_paragraph_wraps_at_its_spaces_under_the_indent() {
        assert_eq!(
            wrapped(
                "The guide to one robot, measured at launch.",
                "  ",
                Some(20)
            ),
            "  The guide to one\n  robot, measured at\n  launch.\n"
        );
        assert_eq!(
            wrapped("The   guide\nto one robot.", "  ", None),
            "  The guide to one robot.\n"
        );
    }

    #[test]
    fn a_word_wider_than_the_line_breaks_at_the_end_of_the_line() {
        assert_eq!(
            wrapped("see park_joint_positions now", "  ", Some(10)),
            "  see\n  park_joi\n  nt_posit\n  ions now\n"
        );
    }

    #[test]
    fn paragraphs_are_apart_by_a_blank_line() {
        let description = DocumentDescription::new(vec![
            Description::new("One.").unwrap(),
            Description::new("Two.").unwrap(),
        ])
        .unwrap();
        assert_eq!(
            paragraphs(&description, "    ", None),
            "    One.\n\n    Two.\n"
        );
    }
}
