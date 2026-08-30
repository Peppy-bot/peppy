//! Box-drawing table primitives shared by the commands that render tables
//! (`stack list`, `stack benchmark`, `repo search`) so they draw the same
//! borders, alignment, ANSI handling, and terminal fitting. Cells may carry
//! embedded newlines: a cell that spans several lines makes the whole row
//! that tall, padding the shorter cells. Under a width budget the widest
//! columns give way first, never below their header's width, and over-long
//! cells wrap onto continuation lines, so the box holds in any terminal.

use std::fmt::Write as _;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::colors::RESET;

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
/// consumed. Shared by [`col_width`] (which measures around the codes) and
/// the tests' `strip_ansi` (which drops them) so the two never disagree on
/// what an escape sequence is.
pub(super) fn skip_csi(chars: &mut std::str::Chars<'_>) {
    take_csi(chars);
}

/// Consumes a CSI escape sequence whose leading `\x1b` has already been
/// taken, returning the consumed characters: an optional `[` introducer,
/// then bytes up to and including a final byte in `@`..=`~`. The `[` itself
/// falls in that range, so it must be taken first or the scan would stop
/// one char too early.
fn take_csi(chars: &mut std::str::Chars<'_>) -> String {
    let mut seq = String::new();
    if chars.clone().next() == Some('[') {
        seq.push('[');
        chars.next();
    }
    for f in chars.by_ref() {
        seq.push(f);
        if ('@'..='~').contains(&f) {
            break;
        }
    }
    seq
}

/// Tracks the SGR span state [`wrap`] carries across line breaks: a reset
/// closes the open span, any other sequence opens one. Spans in the wrapped
/// text are sequential (`code … reset`, the shape every `paint` in this
/// crate emits), which is all this tracking assumes.
fn set_active(active: &mut Option<String>, seq: &str) {
    *active = match seq {
        "\x1b[0m" | "\x1b[m" => None,
        _ => Some(seq.to_owned()),
    };
}

/// Word-wraps `s` (a single line) to `width` display columns, inserting
/// `\n` between lines. Splits on spaces and measures with [`col_width`]; a
/// single token wider than `width` is hard-broken at the column boundary,
/// so no output line ever exceeds it. ANSI SGR escapes are zero-width, and
/// a span left open at a break is closed with a reset and reopened on the
/// next line, so every physical line is independently colored and whatever
/// follows it stays plain.
pub(super) fn wrap(s: &str, width: usize) -> String {
    fn break_line(
        lines: &mut Vec<String>,
        cur: &mut String,
        cur_w: &mut usize,
        active: &Option<String>,
    ) {
        if let Some(code) = active {
            cur.push_str(RESET);
            lines.push(std::mem::take(cur));
            cur.push_str(code);
        } else {
            lines.push(std::mem::take(cur));
        }
        *cur_w = 0;
    }

    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    let mut active: Option<String> = None;

    for word in s.split(' ').filter(|w| !w.is_empty()) {
        let ww = col_width(word);
        if ww <= width {
            if cur_w > 0 && cur_w + 1 + ww > width {
                break_line(&mut lines, &mut cur, &mut cur_w, &active);
            } else if cur_w > 0 {
                cur.push(' ');
                cur_w += 1;
            }
            cur.push_str(word);
            cur_w += ww;
            let mut chars = word.chars();
            while let Some(c) = chars.next() {
                if c == '\x1b' {
                    set_active(&mut active, &format!("\x1b{}", take_csi(&mut chars)));
                }
            }
            continue;
        }
        // A token wider than a whole line: place it character by character,
        // breaking at the column boundary.
        if cur_w > 0 {
            break_line(&mut lines, &mut cur, &mut cur_w, &active);
        }
        let mut chars = word.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                let seq = format!("\x1b{}", take_csi(&mut chars));
                cur.push_str(&seq);
                set_active(&mut active, &seq);
                continue;
            }
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            if cur_w + cw > width {
                break_line(&mut lines, &mut cur, &mut cur_w, &active);
            }
            cur.push(c);
            cur_w += cw;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines.join("\n")
}

/// Re-wraps every line of a (possibly multi-line) cell to `width` display
/// columns with [`wrap`]; lines already inside it pass through untouched.
fn fit_cell(cell: &str, width: usize) -> String {
    cell.split('\n')
        .map(|line| match col_width(line) > width {
            true => wrap(line, width),
            false => line.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders a box-drawing table from a header row and one or more row blocks. A
/// horizontal rule separates consecutive blocks, so a flat table passes a
/// single block (no internal rules) and a grouped one a block per group. Column
/// widths are measured across every cell with [`col_width`] (per line, for
/// multi-line cells), which strips ANSI so a colored cell aligns with its plain
/// text. Under `max_width` (the whole line's budget, outline included) the
/// widest columns give way first, ties rightmost, never below their header's
/// width, and cells wider than their column wrap with [`wrap`] onto
/// continuation lines; `None` leaves every cell line unwrapped. A trailing
/// blank line is appended so callers can stack sections.
pub(super) fn render_table(
    out: &mut String,
    headers: &[&str],
    blocks: &[Vec<Vec<String>>],
    max_width: Option<usize>,
) {
    let mut widths: Vec<usize> = headers.iter().copied().map(col_width).collect();
    for row in blocks.iter().flatten() {
        for (i, cell) in row.iter().enumerate() {
            let w = cell.split('\n').map(col_width).max().unwrap_or(0);
            if i < widths.len() {
                widths[i] = widths[i].max(w);
            }
        }
    }
    if let Some(limit) = max_width {
        // `│ cell │ cell │`: three outline characters per column plus the
        // closing one.
        let budget = limit.saturating_sub(3 * widths.len() + 1);
        let floors: Vec<usize> = headers.iter().copied().map(col_width).collect();
        while widths.iter().sum::<usize>() > budget {
            let Some(column) = (0..widths.len())
                .filter(|&column| widths[column] > floors[column])
                .max_by_key(|&column| (widths[column], column))
            else {
                break;
            };
            widths[column] -= 1;
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
            match max_width {
                Some(_) => {
                    let fitted: Vec<String> = row
                        .iter()
                        .zip(&widths)
                        .map(|(cell, &width)| fit_cell(cell, width))
                        .collect();
                    write_row(out, &fitted, &widths);
                }
                None => write_row(out, row, &widths),
            }
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
    fn wrap_breaks_on_width_keeping_a_fitting_colored_token_whole() {
        let wrapped = wrap("via \x1b[36muvc_camera:v1\x1b[0m implemented", 18);
        // "via uvc_camera:v1" is 17 visible cols; "implemented" then wraps.
        assert_eq!(wrapped, "via \x1b[36muvc_camera:v1\x1b[0m\nimplemented");
    }

    /// A single token wider than a line is hard-broken at the column
    /// boundary, and the span it sits in is closed and reopened around
    /// every break so each physical line is independently colored.
    #[test]
    fn wrap_hard_breaks_an_oversized_token_repainting_every_line() {
        let wrapped = wrap("\x1b[31maaaaaaaaaa\x1b[0m", 4);
        assert_eq!(
            wrapped,
            "\x1b[31maaaa\x1b[0m\n\x1b[31maaaa\x1b[0m\n\x1b[31maa\x1b[0m"
        );
    }

    #[test]
    fn multiline_cell_makes_row_taller_and_stays_aligned() {
        let mut out = String::new();
        let blocks = vec![vec![vec!["a\nbb".to_string(), "x".to_string()]]];
        render_table(&mut out, &["c1", "c2"], &blocks, None);
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

    /// Under a budget the widest column gives way and its over-long cell
    /// wraps, so no line exceeds the budget and the header keeps its width.
    #[test]
    fn render_table_fits_its_width_budget() {
        let mut out = String::new();
        let blocks = vec![vec![vec![
            "n1".to_string(),
            "a/very/long/path/that/overruns.json5".to_string(),
        ]]];
        render_table(&mut out, &["NODE", "PATH"], &blocks, Some(26));
        for line in out.lines() {
            assert!(UnicodeWidthStr::width(line) <= 26, "{line}\n{out}");
        }
        assert!(out.contains("│ NODE │ PATH"), "{out}");
        assert!(
            out.lines().count() > 5,
            "the path wrapped onto more lines: {out}"
        );
    }

    /// A budget narrower than the headers stops shrinking at their widths:
    /// the outline holds at that floor instead of collapsing.
    #[test]
    fn columns_never_shrink_below_their_header() {
        let mut out = String::new();
        let blocks = vec![vec![vec!["x".repeat(30), "y".repeat(30)]]];
        render_table(&mut out, &["NODE", "PATH"], &blocks, Some(5));
        assert!(out.contains("│ NODE │ PATH │"), "{out}");
    }
}
