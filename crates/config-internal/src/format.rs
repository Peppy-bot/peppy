// Lightweight JSON5 formatter that preserves JSON5 features (comments, single quotes,
// unquoted keys, hex numbers, trailing commas) while adding indentation and newlines.
// Assumes valid JSON5 input; returns formatted text.
pub fn prettify_json5(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 2);

    let mut indent: usize = 0;
    let indent_unit = "  "; // 2 spaces per level

    let mut in_string = false;
    let mut string_delim: char = '\0';
    let mut escaped = false;

    let mut in_line_comment = false; // //...
    let mut in_block_comment = false; // /*...*/
    // Track whether we are at the start of a line to emit indentation lazily
    let mut start_of_line = true;

    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == string_delim {
                in_string = false;
                string_delim = '\0';
            }
            continue;
        }

        if in_line_comment {
            // Emit the comment as-is until newline
            out.push(c);
            if c == '\n' {
                in_line_comment = false;
                start_of_line = true;
            }
            continue;
        }

        if in_block_comment {
            // Emit the block comment as-is
            out.push(c);
            if c == '*' {
                if let Some('/') = chars.peek() {
                    // close comment
                    out.push('/');
                    chars.next();
                    in_block_comment = false;
                }
            }
            if c == '\n' {
                start_of_line = true;
            }
            continue;
        }

        match c {
            // Skip insignificant whitespace; we'll manage spacing ourselves
            ' ' | '\t' | '\r' | '\n' => {
                if c == '\n' {
                    // Collapse multiple blank lines
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                    start_of_line = true;
                }
            }

            // Comments
            '/' => {
                if let Some(next) = chars.peek().copied() {
                    match next {
                        '/' => {
                            // Start of line comment
                            if !out.ends_with('\n') {
                                out.push('\n');
                            }
                            start_of_line = true;
                            emit_indent(&mut out, indent, indent_unit, &mut start_of_line);
                            out.push('/');
                            out.push('/');
                            chars.next();
                            in_line_comment = true;
                        }
                        '*' => {
                            // Start of block comment
                            if !out.ends_with('\n') {
                                out.push('\n');
                            }
                            start_of_line = true;
                            emit_indent(&mut out, indent, indent_unit, &mut start_of_line);
                            out.push('/');
                            out.push('*');
                            chars.next();
                            in_block_comment = true;
                        }
                        _ => {
                            emit_indent(&mut out, indent, indent_unit, &mut start_of_line);
                            out.push('/');
                        }
                    }
                } else {
                    emit_indent(&mut out, indent, indent_unit, &mut start_of_line);
                    out.push('/');
                }
            }

            // Strings
            '"' | '\'' => {
                emit_indent(&mut out, indent, indent_unit, &mut start_of_line);
                in_string = true;
                string_delim = c;
                out.push(c);
            }

            // Opening containers
            '{' | '[' => {
                emit_indent(&mut out, indent, indent_unit, &mut start_of_line);
                out.push(c);
                indent += 1;
                out.push('\n');
                start_of_line = true;
            }

            // Closing containers
            '}' | ']' => {
                if indent > 0 {
                    indent -= 1;
                }
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                start_of_line = true;
                emit_indent(&mut out, indent, indent_unit, &mut start_of_line);
                out.push(c);
            }

            // Key-value separator
            ':' => {
                out.push(':');
                out.push(' ');
                start_of_line = false;
            }

            // Element separator
            ',' => {
                out.push(',');
                out.push('\n');
                start_of_line = true;
            }

            // Everything else (numbers, identifiers, literals, signs, etc.)
            _ => {
                emit_indent(&mut out, indent, indent_unit, &mut start_of_line);
                out.push(c);
            }
        }
    }

    out
}

#[inline]
fn emit_indent(out: &mut String, indent: usize, unit: &str, start_of_line: &mut bool) {
    if *start_of_line {
        for _ in 0..indent {
            out.push_str(unit);
        }
        *start_of_line = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_simple_object() {
        let input = r#"{a:1,b:2}"#;
        let out = prettify_json5(input);
        assert!(out.contains("\n  a: 1,"));
        assert!(out.contains("\n  b: 2\n"));
    }

    #[test]
    fn preserves_strings_and_comments() {
        let input = r#"{ // c1
// c2
a: 'x\'y', /* c3 */ b: 0x10, list: [1,2,3,], }"#;
        let out = prettify_json5(input);
        assert!(out.contains("// c1"));
        assert!(out.contains("// c2"));
        assert!(out.contains("/* c3 */"));
        assert!(out.contains("0x10"));
        assert!(out.contains("[\n    1,\n    2,\n    3,\n  ]"));
    }
}
