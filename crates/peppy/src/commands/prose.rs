//! The descriptions of documents as the commands print them: each paragraph
//! wrapped at its spaces to the width of the terminal, under an indent.

use unicode_width::UnicodeWidthStr;

/// `text` as lines that start with `indent`, wrapped at its spaces so no
/// line is wider than `max_width` columns; a word that does not fit stays
/// whole on a line of its own. `None` keeps the text on one line. The
/// spaces of `text` collapse to one, and each line ends with a newline.
pub(crate) fn wrapped(text: &str, indent: &str, max_width: Option<usize>) -> String {
    let budget = max_width.map(|width| width.saturating_sub(indent.width()));
    let mut out = String::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let fits = budget.is_none_or(|budget| line.width() + 1 + word.width() <= budget);
        if !line.is_empty() && !fits {
            out.push_str(&format!("{indent}{line}\n"));
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push_str(&format!("{indent}{line}\n"));
    }
    out
}

/// The paragraphs of a document description, each [`wrapped`], with a
/// blank line between two paragraphs.
pub(crate) fn paragraphs<'a>(
    paragraphs: impl IntoIterator<Item = &'a str>,
    indent: &str,
    max_width: Option<usize>,
) -> String {
    paragraphs
        .into_iter()
        .map(|paragraph| wrapped(paragraph, indent, max_width))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_word_wider_than_the_line_stays_whole() {
        assert_eq!(
            wrapped("see park_joint_positions now", "", Some(8)),
            "see\npark_joint_positions\nnow\n"
        );
    }

    #[test]
    fn paragraphs_are_apart_by_a_blank_line() {
        assert_eq!(
            paragraphs(["One.", "Two."], "    ", None),
            "    One.\n\n    Two.\n"
        );
    }
}
