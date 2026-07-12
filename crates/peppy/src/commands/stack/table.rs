//! Box-drawing table primitives shared by the `stack` subcommands so
//! `stack list` and `stack benchmark` render with the same borders, alignment,
//! and ANSI handling. Cells may carry embedded newlines: a cell that spans
//! several lines makes the whole row that tall, padding the shorter cells;
//! used by `stack benchmark` to wrap its wide `edge` cell instead of letting
//! the table overflow a narrow terminal.

use std::fmt::Write as _;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Terminal display width of a cell. Column widths, box-drawing borders, and
/// cell padding must all measure the same way or the table skews on non-ASCII
/// content: a wide CJK glyph or a Unicode path counts as more bytes than
/// display columns, and a combining mark as fewer. Routing every measurement
/// through this keeps the three in agreement.
///
/// ANSI SGR escapes (the color codes `paint` injects) occupy zero display
/// columns, so they are skipped here; otherwise a colored cell would measure
/// wider than its plain text and skew the box against the borders. A cell
/// argument must be a single line; callers split on `\n` first.
pub(super) fn col_width(s: &str) -> usize {
    if !s.as_bytes().contains(&0x1b) {
        return UnicodeWidthStr::width(s);
    }
    let mut width = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            skip_csi(&mut chars);
        } else {
            width += UnicodeWidthChar::width(c).unwrap_or(0);
        }
    }
    width
}

/// Advances past a CSI escape sequence whose leading `\x1b` has already been
/// consumed: an optional `[` introducer, then bytes up to and including a final
/// byte in `@`..=`~`. The `[` itself falls in that range, so it must be skipped
/// first or the scan would stop one char too early. Shared by [`col_width`]
/// (which measures around the codes) and the tests' `strip_ansi` (which drops
/// them) so the two never disagree on what an escape sequence is.
pub(super) fn skip_csi(chars: &mut std::str::Chars<'_>) {
    if chars.clone().next() == Some('[') {
        chars.next();
    }
    for f in chars.by_ref() {
        if ('@'..='~').contains(&f) {
            break;
        }
    }
}

/// Word-wrap `s` to `width` display columns, inserting `\n` between lines.
/// Splits on spaces and measures with [`col_width`], so a colored token (whose
/// ANSI escapes are zero-width and contain no space) wraps as one unit and is
/// never split mid-escape. A single token wider than `width` overflows its line
/// rather than being broken; fine for the short tokens this wraps.
pub(super) fn wrap_ansi(s: &str, width: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0;
    for word in s.split(' ').filter(|w| !w.is_empty()) {
        let ww = col_width(word);
        if cur.is_empty() {
            cur.push_str(word);
            cur_w = ww;
        } else if cur_w + 1 + ww <= width {
            cur.push(' ');
            cur.push_str(word);
            cur_w += 1 + ww;
        } else {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(word);
            cur_w = ww;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines.join("\n")
}

/// Renders a box-drawing table from a header row and one or more row blocks. A
/// horizontal rule separates consecutive blocks, so a flat table passes a
/// single block (no internal rules) and a grouped one a block per group. Column
/// widths are measured across every cell with [`col_width`] (per line, for
/// multi-line cells), which strips ANSI so a colored cell aligns with its plain
/// text. A trailing blank line is appended so callers can stack sections.
pub(super) fn render_table(out: &mut String, headers: &[&str], blocks: &[Vec<Vec<String>>]) {
    let mut widths: Vec<usize> = headers.iter().copied().map(col_width).collect();
    for row in blocks.iter().flatten() {
        for (i, cell) in row.iter().enumerate() {
            let w = cell.split('\n').map(col_width).max().unwrap_or(0);
            if i < widths.len() {
                widths[i] = widths[i].max(w);
            }
        }
    }

    write_border(out, &widths, '┌', '┬', '┐');
    let header_row: Vec<String> = headers.iter().copied().map(String::from).collect();
    write_row(out, &header_row, &widths);
    write_border(out, &widths, '├', '┼', '┤');
    for (block_idx, block) in blocks.iter().enumerate() {
        if block_idx > 0 {
            write_border(out, &widths, '├', '┼', '┤');
        }
        for row in block {
            write_row(out, row, &widths);
        }
    }
    write_border(out, &widths, '└', '┴', '┘');
    let _ = writeln!(out);
}

fn write_border(out: &mut String, widths: &[usize], left: char, sep: char, right: char) {
    let _ = write!(out, "{}", left);
    for (i, w) in widths.iter().enumerate() {
        for _ in 0..(w + 2) {
            let _ = write!(out, "─");
        }
        let _ = write!(out, "{}", if i + 1 == widths.len() { right } else { sep });
    }
    let _ = writeln!(out);
}

/// Writes one logical row, which may span several physical lines when a cell
/// carries embedded newlines. The row's height is its tallest cell; each
/// physical line pads every cell to its column width, blank-filling cells that
/// have fewer lines so the borders stay aligned.
fn write_row(out: &mut String, cells: &[String], widths: &[usize]) {
    let split: Vec<Vec<&str>> = cells.iter().map(|c| c.split('\n').collect()).collect();
    let height = split.iter().map(|c| c.len()).max().unwrap_or(1);
    for line in 0..height {
        let _ = write!(out, "│");
        for (i, w) in widths.iter().enumerate() {
            let cell = split
                .get(i)
                .and_then(|l| l.get(line))
                .copied()
                .unwrap_or("");
            // Pad by display columns, not `char` count: `{:<width$}` would
            // mis-pad wide/zero-width glyphs and skew the box against the
            // `col_width`-based widths and borders.
            let pad = w.saturating_sub(col_width(cell));
            let _ = write!(out, " {}{} │", cell, " ".repeat(pad));
        }
        let _ = writeln!(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn col_width_skips_ansi() {
        assert_eq!(col_width("\x1b[36mabc\x1b[0m"), 3);
        assert_eq!(col_width("abc"), 3);
    }

    #[test]
    fn wrap_ansi_breaks_on_width_keeping_colored_token_whole() {
        let wrapped = wrap_ansi("via \x1b[36muvc_camera:v1\x1b[0m implemented", 18);
        // "via uvc_camera:v1" is 17 visible cols; "implemented" then wraps.
        assert_eq!(wrapped, "via \x1b[36muvc_camera:v1\x1b[0m\nimplemented");
    }

    #[test]
    fn multiline_cell_makes_row_taller_and_stays_aligned() {
        let mut out = String::new();
        let blocks = vec![vec![vec!["a\nbb".to_string(), "x".to_string()]]];
        render_table(&mut out, &["c1", "c2"], &blocks);
        let lines: Vec<&str> = out.lines().collect();
        // Header + two physical body lines; the second body line blank-fills c2.
        assert!(lines.iter().any(|l| l.contains("│ a  │ x  │")));
        assert!(lines.iter().any(|l| l.contains("│ bb │    │")));
        // Every box line shares one display width.
        let widths: Vec<usize> = lines
            .iter()
            .filter(|l| l.starts_with(['┌', '├', '└', '│']))
            .map(|l| UnicodeWidthStr::width(*l))
            .collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "rows misaligned: {widths:?}"
        );
    }
}
